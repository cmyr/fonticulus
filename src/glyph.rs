use crate::utils::is_all_same;
use fonttools::glyf;
use fonttools::glyf::contourutils::kurbo_contour_to_glyf_contour;
use fonttools::gvar::DeltaSet;
use fonttools::gvar::GlyphVariationData;
use fonttools::otvar::VariationModel;
use kurbo::{BezPath, PathEl, PathSeg};
use std::collections::BTreeMap;
use unzip_n::unzip_n;

unzip_n!(3);

type GlyphContour = Vec<Vec<glyf::Point>>;

// OK, this is the trickiest portion of the project to follow.
//
// We are going to be converting a set of layers representing a single glyph
// at different points in the designspace into a base `glyf` table entry plus
// `gvar` table information.
//
// We are being handed:
pub fn layers_to_glyph(
    // The index of the default master (this tells us which outline goes into
    // the `glyf` table),
    default_master: usize,
    // A mapping of glyph names to glyph IDs (for resolving components)
    mapping: &BTreeMap<String, u16>,
    // The set of layers
    layers: &[Option<&babelfont::Layer>],
    // A variation model, which tells us where all the layers live in the
    // design space
    model: Option<&VariationModel>,
    // and the glyph's name, for debugging purposes
    glif_name: &str,
) -> (glyf::Glyph, Option<GlyphVariationData>) {
    let mut glyph = glyf::Glyph {
        xMin: 0,
        xMax: 0,
        yMin: 0,
        yMax: 0,
        contours: vec![],
        instructions: vec![],
        components: vec![],
        overlap: false,
    };

    let default_layer = layers
        .get(default_master)
        .expect("Couldn't index the default master into the layer list?!")
        .expect("No glif in default master!");

    /* Dispatch empty glyphs (space, etc.) straight away */
    if default_layer.components().is_empty() && default_layer.paths().is_empty() {
        return (glyph, None);
    }

    /* Do components */
    for component in &default_layer.components() {
        if let Some(glyf_component) = babelfont_component_to_glyf_component(component, mapping) {
            glyph.components.push(glyf_component);
        }
        /* Ideally at this point we would have decomposed mixed glyphs and we
        could go home here, but because that is still a todo item in
        buildbasic, we continue, hackily. */
        // XXX Oops, we also need to compute variations on the component positions
    }

    /* Now we will do the outlines */

    /* Handle the simple case of a static font. */
    if model.is_none() {
        for (contour_ix, contour) in default_layer.paths().iter().enumerate() {
            let glyph_contour =
                babelfont_contours_to_glyf_contours(contour_ix, vec![contour], 0, glif_name)
                    .first()
                    .unwrap()
                    .clone();
            glyph.contours.push(glyph_contour);
        }
        return (glyph, None);
    }

    /* OK, we're doing the contours of a variable font. Some of the masters
    may be sparse, i.e. not containing a layer for this glyph. We will
    keep the indices around for when we have to filter them out. */
    let indexes_of_nonsparse_masters: Vec<usize> =
        (0..layers.len()).filter(|x| layers[*x].is_some()).collect();

    let mut widths: Vec<Option<i32>> = vec![];
    let mut contours: Vec<Option<GlyphContour>> = vec![];

    for o in layers {
        widths.push(o.and_then(|x| Some(x.width)));
        contours.push(o.and_then(|_| Some(vec![])));
    }

    // Convert each contour in turn (across layers)
    for (index, _) in default_layer.paths().iter().enumerate() {
        for o in layers {
            // If this contour doesn't exist in a given layer, we have a problem
            if o.is_some() && index >= o.unwrap().paths().len() {
                log::error!("Incompatible contour count in glyph {:}", glif_name);
                return (glyph, None);
            }
        }

        // List of all contours across *non-sparse* layers.
        let all_contours: Vec<&babelfont::Path> = layers
            .iter()
            .filter(|g| g.is_some())
            .map(|x| x.unwrap().paths()[index])
            .collect();

        // Convert them together into OT contours
        let all_glyf_contours =
            babelfont_contours_to_glyf_contours(index, all_contours, default_master, glif_name);

        // Now we put them into their respective master
        for (finished_contour, &master_id) in all_glyf_contours
            .iter()
            .zip(indexes_of_nonsparse_masters.iter())
        {
            assert!(contours[master_id].is_some());
            contours[master_id]
                .as_mut()
                .unwrap()
                .push(finished_contour.clone());
        }
    }

    // Now generate variations
    if layers.len() > 1
        && !contours.is_empty()
        && !contours[default_master].as_ref().unwrap().is_empty()
    {
        if !glyph.components.is_empty() {
            log::warn!("Can't create gvar deltas for mixed glyph {:}", glif_name);
            return (glyph, None);
        }

        // Gather all contour lengths, ensure they are the same.
        // XXX should be caught in babelfont_contours_to_glyf_contours?
        let lengths: Vec<usize> = contours
            .iter()
            .filter(|x| x.is_some())
            .map(|g| g.as_ref().unwrap().iter().flatten().count())
            .collect();
        if !is_all_same(&lengths) {
            log::warn!("Incompatible glyph: {:}, lengths: {:?}", glif_name, lengths);
            glyph.contours = contours[default_master].as_ref().unwrap().clone();
            return (glyph, None);
        }

        // We have everything we need
        let deltas = compute_deltas(&contours, widths, model.unwrap());
        glyph.contours = contours[default_master].as_ref().unwrap().clone();
        return (glyph, Some(deltas));
    }

    (glyph, None)
}

fn babelfont_contours_to_glyf_contours(
    // Which path this is in the glyph (for error reporting)
    path_index: usize,

    // A (non-sparse) list of contours
    paths: Vec<&babelfont::Path>,

    // The index of the default master (used as the reference for curve construction)
    default_master: usize,

    // Which glyph this is (for error reporting)
    glif_name: &str,
) -> Vec<Vec<glyf::Point>> {
    // Let's first get them all to kurbo elements.
    let kurbo_paths: Vec<BezPath> = paths
        .iter()
        .map(|x| x.to_kurbo().expect("Bad contour construction"))
        .collect();

    // Ensure they are all the same size
    let lengths: Vec<usize> = kurbo_paths.iter().map(|x| x.elements().len()).collect();

    if !is_all_same(&lengths) {
        log::error!(
            "Incompatible contour {:} in glyph {:}: {:?}",
            path_index,
            glif_name,
            lengths
        );
        return vec![];
    }

    // XXX ensure they are all compatible, type-wise.

    // We're going to turn the list of cubic bezpaths into quadratic bezpaths
    let mut quadratic_paths: Vec<kurbo::BezPath> = paths.iter().map(|_| BezPath::new()).collect();

    let default_elements: &[PathEl] = kurbo_paths[default_master].elements();
    for (el_ix, el) in default_elements.iter().enumerate() {
        match el {
            PathEl::CurveTo(_, _, _) => {
                // Convert all the cubics to quadratics in one go, across layers
                let all_curves: Vec<PathSeg> = kurbo_paths
                    .iter()
                    .map(|x| x.get_seg(el_ix).unwrap())
                    .collect();
                let all_quadratics = cubics_to_quadratics(all_curves, glif_name);
                for (c_ix, contour) in quadratic_paths.iter_mut().enumerate() {
                    for &quad in &all_quadratics[c_ix] {
                        contour.push(quad);
                    }
                }
            }
            _ => {
                for (c_ix, contour) in quadratic_paths.iter_mut().enumerate() {
                    contour.push(kurbo_paths[c_ix].elements()[el_ix]);
                }
            }
        }
    }

    // Now turn our kurbo quadratic bezpaths into glyf contours
    quadratic_paths
        .iter()
        .map(|x| {
            let mut c = kurbo_contour_to_glyf_contour(x, 0.5);
            // Ideally we should remove any implied oncurves here, but only if
            // we can do it in an interpolation-compatible way! See notes below.
            // remove_implied_oncurves(&mut c);
            c
        })
        .collect()
}

// Convert a list of cubic *segments* (not contours, individual cubic curves)
// to quadratic segments, in an interpolation-compatible way. Unfortunately,
// because we are working on individual curves, this is not aware of the
// existence of implied oncurves.
fn cubics_to_quadratics(cubics: Vec<PathSeg>, _glif_name: &str) -> Vec<Vec<PathEl>> {
    let mut error = 0.05;
    let mut warned = false;
    while error < 50.0 {
        let mut quads: Vec<Vec<kurbo::PathEl>> = vec![];

        // Try it once
        for pathseg in &cubics {
            if let PathSeg::Cubic(cubic) = pathseg {
                quads.push(
                    cubic
                        .to_quads(error)
                        .map(|(_, _, x)| PathEl::QuadTo(x.p1, x.p2))
                        .collect(),
                )
            } else {
                // log::error!("Incompatible contours in glyph {:?}", glif_name);
                return itertools::repeat_n(vec![], cubics.len()).collect();
            }
        }

        // Was it a compatible collection of quads?
        let lengths: Vec<usize> = quads.iter().map(|x| x.len()).collect();
        if is_all_same(&lengths) {
            return quads;
        }

        error *= 1.5; // Exponential backoff
        if error > 20.0 && !warned {
            // log::warn!(
            //     "{:} is proving difficult to interpolate - consider redesigning?",
            //     glif_name
            // );
            warned = true;
        }
    }
    panic!("Couldn't compatibly interpolate contours");
}

fn babelfont_component_to_glyf_component(
    component: &babelfont::Component,
    mapping: &BTreeMap<String, u16>,
) -> Option<glyf::Component> {
    if let Some(&glyph_index) = mapping.get(&component.reference) {
        Some(glyf::Component {
            glyph_index,
            match_points: None,
            flags: glyf::ComponentFlags::empty(),
            transformation: component.transform,
        })
    } else {
        log::warn!("Couldn't find component for {:?}", component.reference);
        None
    }
}

fn compute_deltas(
    contours: &[Option<GlyphContour>],
    widths: Vec<Option<i32>>,
    model: &VariationModel,
) -> GlyphVariationData {
    let mut deltasets: Vec<DeltaSet> = vec![];
    let mut all_coords = vec![];

    for (ix, master) in contours.iter().enumerate() {
        if let Some(master) = master {
            // If this is not a sparse master, we have a width and a set of coordinates.
            let width = widths[ix].unwrap();
            // Flatten all points (i.e. combine all contours together) in the glyph
            // and split up X and Y into separate arrays.
            let (mut master_x_coords, mut master_y_coords): (Vec<f32>, Vec<f32>) = master
                .iter()
                .flatten()
                .map(|pt| (pt.x as f32, pt.y as f32))
                .unzip();

            // Add the phantom points
            master_x_coords.extend(vec![0_f32, width as f32, 0.0, 0.0]);
            master_y_coords.extend(vec![0.0, 0.0, 0.0, 0.0]);

            // Concat the X-coordinates/Y-coordinates in preparation for being
            // reshaped into a 2d ndarray.
            let len = master_x_coords.len();
            master_x_coords.extend(master_y_coords);

            all_coords.push(Some(
                ndarray::Array2::from_shape_vec((2, len), master_x_coords)
                    .unwrap()
                    .reversed_axes(),
            ));
        } else {
            all_coords.push(None);
        }
    }

    // The model takes Vec<T> T:Sub, and ndarray::Array2 implements Sub,
    // so we can just send the whole vec of ndarrays to the model and get
    // back our deltas.
    let deltas_and_supports = model.get_deltas_and_supports(&all_coords);

    for (delta, support) in deltas_and_supports.iter() {
        if support.is_empty() {
            continue;
        }

        // Turn the ndarray back into a vec of tuples
        let deltas: Vec<(i16, i16)> = delta
            .mapv(|x| x as i16)
            .outer_iter()
            .map(|x| (x[0], x[1]))
            .collect();

        // The variation model gives us the tents for each deltaset
        let tuples = model
            .axis_order
            .iter()
            .map(|ax| support.get(ax).unwrap_or(&(0.0, 0.0, 0.0)))
            .copied();
        let (start, peak, end) = tuples.into_iter().unzip_n_vec();

        // And we're done
        deltasets.push(DeltaSet {
            peak,
            start,
            end,
            deltas,
        })
    }
    GlyphVariationData { deltasets }
}
