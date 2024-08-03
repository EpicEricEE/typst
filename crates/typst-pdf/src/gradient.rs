use std::collections::HashMap;
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use ecow::eco_format;
use pdf_writer::types::{MaskType, PaintType, TilingType};
use pdf_writer::writers::ExtGraphicsState;
use pdf_writer::{
    types::{ColorSpaceOperand, FunctionShadingType},
    writers::StreamShadingType,
    Filter, Finish, Name, Ref,
};
use pdf_writer::{Content, Rect};

use typst::layout::{Abs, Angle, Point, Quadrant, Ratio, Size, Transform};
use typst::utils::Numeric;
use typst::visualize::{
    Color, ColorSpace, Gradient, Luma, RatioOrAngle, RelativeTo, WeightedColor,
};

use crate::color::{self, ColorSpaceExt, PaintEncode, QuantizedColor};
use crate::{content, WithGlobalRefs};
use crate::{deflate, transform_to_array, AbsExt, PdfChunk};

/// A unique-transform-aspect-ratio combination that will be encoded into the
/// PDF.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct PdfGradient {
    /// The transform to apply to the gradient.
    pub transform: Transform,
    /// The aspect ratio of the gradient.
    /// Required for aspect ratio correction.
    pub aspect_ratio: Ratio,
    /// The gradient.
    pub gradient: Gradient,
    /// The corrected angle of the gradient.
    pub angle: Angle,
    /// The size of the page this gradient is on.
    pub page_size: Size,
}

/// Writes the actual gradients (shading patterns) to the PDF.
/// This is performed once after writing all pages.
pub fn write_gradients(
    context: &WithGlobalRefs,
) -> (PdfChunk, HashMap<PdfGradient, Ref>) {
    let mut chunk = PdfChunk::new();
    let mut out = HashMap::new();
    context.resources.traverse(&mut |resources| {
        for pdf_gradient in resources.gradients.items() {
            if out.contains_key(pdf_gradient) {
                continue;
            }

            let shading_pattern = write_gradient(
                context,
                pdf_gradient,
                &mut chunk,
                color_space_of(&pdf_gradient.gradient),
            );

            let pattern = if pdf_gradient.gradient.is_transparent() {
                transparent_tiling(context, pdf_gradient, shading_pattern, &mut chunk)
            } else {
                shading_pattern
            };

            out.insert(pdf_gradient.clone(), pattern);
        }
    });

    (chunk, out)
}

/// Writes a tiling pattern for transparent gradients.
fn transparent_tiling<'a>(
    context: &WithGlobalRefs,
    pdf_gradient: &PdfGradient,
    shading_pattern: Ref,
    chunk: &'a mut PdfChunk,
) -> Ref {
    const PATTERN_NAME: Name = Name(b"Gr");
    const EXTGSTATE_NAME: Name = Name(b"Gs");

    let page_width = pdf_gradient.page_size.x.to_f32();
    let page_height = pdf_gradient.page_size.y.to_f32();

    // Transform the gradient to a grayscale alpha gradient.
    let alpha_gradient = {
        let mut alpha_gradient = pdf_gradient.clone();

        let to_alpha = |stops: &mut Vec<(Color, _)>| {
            stops.iter_mut().for_each(|(color, _)| {
                let alpha = color.alpha().unwrap_or(1.0);
                *color = Color::from(Luma::new(alpha, 1.0));
            });
        };

        match &mut alpha_gradient.gradient {
            Gradient::Linear(linear) => {
                Arc::make_mut(linear).space = ColorSpace::D65Gray;
                to_alpha(&mut Arc::make_mut(linear).stops);
            }
            Gradient::Radial(radial) => {
                Arc::make_mut(radial).space = ColorSpace::D65Gray;
                to_alpha(&mut Arc::make_mut(radial).stops);
            }
            Gradient::Conic(conic) => {
                Arc::make_mut(conic).space = ColorSpace::D65Gray;
                to_alpha(&mut Arc::make_mut(conic).stops);
            }
        }

        alpha_gradient
    };

    // Write the alpha gradient.
    let alpha_shading_pattern = write_gradient(
        context,
        &alpha_gradient,
        chunk,
        color_space_of(&alpha_gradient.gradient),
    );

    // Write the soft mask group.
    // The content of the group is the alpha gradient filled on the full page.
    let mut content = Content::new();
    content
        .set_fill_color_space(ColorSpaceOperand::Pattern)
        .set_fill_pattern(None, PATTERN_NAME)
        .rect(0.0, 0.0, page_width, page_height)
        .fill_nonzero();
    let content = content.finish();

    let soft_mask_ref = chunk.alloc();
    let mut soft_mask = chunk.form_xobject(soft_mask_ref, &content);
    let mut group = soft_mask.bbox(Rect::new(0.0, 0.0, page_width, page_height)).group();
    let color_space = group.transparency().color_space();
    color::write(
        alpha_gradient.gradient.space(),
        color_space,
        &context.globals.color_functions,
    );
    group.finish();
    soft_mask.resources().patterns().pair(PATTERN_NAME, alpha_shading_pattern);
    soft_mask.finish();

    // Write the actual tiling pattern.
    // The content of this pattern is the real gradient drawn on the full page.
    let mut content = Content::new();
    content
        .set_parameters(EXTGSTATE_NAME)
        .set_fill_color_space(ColorSpaceOperand::Pattern)
        .set_fill_pattern(None, PATTERN_NAME)
        .rect(0.0, 0.0, page_width, page_height)
        .fill_nonzero();
    let content = content.finish();

    // The pattern itself is just a single tile with the size of the page.
    let pattern_ref = chunk.alloc();
    let mut pattern = chunk.tiling_pattern(pattern_ref, &content);
    pattern
        .paint_type(PaintType::Colored)
        .tiling_type(TilingType::NoDistortion)
        .bbox(Rect::new(0.0, 0.0, page_width, page_height))
        .x_step(page_width)
        .y_step(page_height);

    let mut resources = pattern.resources();
    resources.patterns().pair(PATTERN_NAME, shading_pattern);
    resources
        .ext_g_states()
        .insert(EXTGSTATE_NAME)
        .start::<ExtGraphicsState>()
        .soft_mask()
        .subtype(MaskType::Luminosity)
        .group(soft_mask_ref);

    pattern_ref
}

/// Writes a shading single gradient.
fn write_gradient(
    context: &WithGlobalRefs,
    pdf_gradient: &PdfGradient,
    chunk: &mut PdfChunk,
    color_space: ColorSpace,
) -> Ref {
    let shading = chunk.alloc();

    let PdfGradient { aspect_ratio, gradient, angle, .. } = pdf_gradient;

    let mut shading_pattern = match &gradient {
        Gradient::Linear(_) => {
            let shading_function = shading_function(gradient, chunk, color_space);
            let mut shading_pattern = chunk.shading_pattern(shading);
            let mut shading = shading_pattern.function_shading();
            shading.shading_type(FunctionShadingType::Axial);

            color::write(
                color_space,
                shading.color_space(),
                &context.globals.color_functions,
            );

            let (mut sin, mut cos) = (angle.sin(), angle.cos());

            // Scale to edges of unit square.
            let factor = cos.abs() + sin.abs();
            sin *= factor;
            cos *= factor;

            let (x1, y1, x2, y2): (f64, f64, f64, f64) = match angle.quadrant() {
                Quadrant::First => (0.0, 0.0, cos, sin),
                Quadrant::Second => (1.0, 0.0, cos + 1.0, sin),
                Quadrant::Third => (1.0, 1.0, cos + 1.0, sin + 1.0),
                Quadrant::Fourth => (0.0, 1.0, cos, sin + 1.0),
            };

            shading
                .anti_alias(gradient.anti_alias())
                .function(shading_function)
                .coords([x1 as f32, y1 as f32, x2 as f32, y2 as f32])
                .extend([true; 2]);

            shading.finish();
            shading_pattern
        }
        Gradient::Radial(radial) => {
            let shading_function = shading_function(gradient, chunk, color_space);
            let mut shading_pattern = chunk.shading_pattern(shading);
            let mut shading = shading_pattern.function_shading();
            shading.shading_type(FunctionShadingType::Radial);

            color::write(
                color_space,
                shading.color_space(),
                &context.globals.color_functions,
            );

            shading
                .anti_alias(gradient.anti_alias())
                .function(shading_function)
                .coords([
                    radial.focal_center.x.get() as f32,
                    radial.focal_center.y.get() as f32,
                    radial.focal_radius.get() as f32,
                    radial.center.x.get() as f32,
                    radial.center.y.get() as f32,
                    radial.radius.get() as f32,
                ])
                .extend([true; 2]);

            shading.finish();
            shading_pattern
        }
        Gradient::Conic(_) => {
            let vertices = compute_vertex_stream(gradient, *aspect_ratio);

            let stream_shading_id = chunk.alloc();
            let mut stream_shading = chunk.stream_shading(stream_shading_id, &vertices);

            color::write(
                color_space,
                stream_shading.color_space(),
                &context.globals.color_functions,
            );

            let range = color_space.range();
            stream_shading
                .bits_per_coordinate(16)
                .bits_per_component(16)
                .bits_per_flag(8)
                .shading_type(StreamShadingType::CoonsPatch)
                .decode([0.0, 1.0, 0.0, 1.0].into_iter().chain(range.iter().copied()))
                .anti_alias(gradient.anti_alias())
                .filter(Filter::FlateDecode);

            stream_shading.finish();

            let mut shading_pattern = chunk.shading_pattern(shading);
            shading_pattern.shading_ref(stream_shading_id);
            shading_pattern
        }
    };

    shading_pattern.matrix(transform_to_array(pdf_gradient.transform));
    shading
}

/// Writes an expotential or stitched function that expresses the gradient.
fn shading_function(
    gradient: &Gradient,
    chunk: &mut PdfChunk,
    color_space: ColorSpace,
) -> Ref {
    let function = chunk.alloc();
    let mut functions = vec![];
    let mut bounds = vec![];
    let mut encode = vec![];

    // Create the individual gradient functions for each pair of stops.
    for window in gradient.stops_ref().windows(2) {
        let (first, second) = (window[0], window[1]);

        // If we have a hue index, we will create several stops in-between
        // to make the gradient smoother without interpolation issues with
        // native color spaces.
        let mut last_c = first.0;
        if gradient.space().hue_index().is_some() {
            for i in 0..=32 {
                let t = i as f64 / 32.0;
                let real_t = first.1.get() * (1.0 - t) + second.1.get() * t;

                let c = gradient.sample(RatioOrAngle::Ratio(Ratio::new(real_t)));
                functions.push(single_gradient(chunk, last_c, c, color_space));
                bounds.push(real_t as f32);
                encode.extend([0.0, 1.0]);
                last_c = c;
            }
        }

        bounds.push(second.1.get() as f32);
        functions.push(single_gradient(chunk, first.0, second.0, color_space));
        encode.extend([0.0, 1.0]);
    }

    // Special case for gradients with only two stops.
    if functions.len() == 1 {
        return functions[0];
    }

    // Remove the last bound, since it's not needed for the stitching function.
    bounds.pop();

    // Create the stitching function.
    chunk
        .stitching_function(function)
        .domain([0.0, 1.0])
        .range(color_space.range().iter().copied())
        .functions(functions)
        .bounds(bounds)
        .encode(encode);

    function
}

/// Writes an expontential function that expresses a single segment (between two
/// stops) of a gradient.
fn single_gradient(
    chunk: &mut PdfChunk,
    first_color: Color,
    second_color: Color,
    color_space: ColorSpace,
) -> Ref {
    let reference = chunk.alloc();
    chunk
        .exponential_function(reference)
        .range(color_space.range().iter().copied())
        .c0(color_space.convert(first_color))
        .c1(color_space.convert(second_color))
        .domain([0.0, 1.0])
        .n(1.0);

    reference
}

impl PaintEncode for Gradient {
    fn set_as_fill(
        &self,
        ctx: &mut content::Builder,
        on_text: bool,
        transforms: content::Transforms,
    ) {
        ctx.reset_fill_color_space();

        let index = register_gradient(ctx, self, on_text, transforms);
        let id = eco_format!("Gr{index}");
        let name = Name(id.as_bytes());

        ctx.content.set_fill_color_space(ColorSpaceOperand::Pattern);
        ctx.content.set_fill_pattern(None, name);
    }

    fn set_as_stroke(
        &self,
        ctx: &mut content::Builder,
        on_text: bool,
        transforms: content::Transforms,
    ) {
        ctx.reset_stroke_color_space();

        let index = register_gradient(ctx, self, on_text, transforms);
        let id = eco_format!("Gr{index}");
        let name = Name(id.as_bytes());

        ctx.content.set_stroke_color_space(ColorSpaceOperand::Pattern);
        ctx.content.set_stroke_pattern(None, name);
    }
}

/// Deduplicates a gradient to a named PDF resource.
fn register_gradient(
    ctx: &mut content::Builder,
    gradient: &Gradient,
    on_text: bool,
    mut transforms: content::Transforms,
) -> usize {
    // Edge cases for strokes.
    if transforms.size.x.is_zero() {
        transforms.size.x = Abs::pt(1.0);
    }

    if transforms.size.y.is_zero() {
        transforms.size.y = Abs::pt(1.0);
    }
    let size = match gradient.unwrap_relative(on_text) {
        RelativeTo::Self_ => transforms.size,
        RelativeTo::Parent => transforms.container_size,
    };

    let (offset_x, offset_y) = match gradient {
        Gradient::Conic(conic) => (
            -size.x * (1.0 - conic.center.x.get() / 2.0) / 2.0,
            -size.y * (1.0 - conic.center.y.get() / 2.0) / 2.0,
        ),
        _ => (Abs::zero(), Abs::zero()),
    };

    let rotation = gradient.angle().unwrap_or_else(Angle::zero);

    let transform = match gradient.unwrap_relative(on_text) {
        RelativeTo::Self_ => transforms.transform,
        RelativeTo::Parent => transforms.container_transform,
    };

    let scale_offset = match gradient {
        Gradient::Conic(_) => 4.0_f64,
        _ => 1.0,
    };

    let pdf_gradient = PdfGradient {
        aspect_ratio: size.aspect_ratio(),
        transform: transform
            .pre_concat(Transform::translate(
                offset_x * scale_offset,
                offset_y * scale_offset,
            ))
            .pre_concat(Transform::scale(
                Ratio::new(size.x.to_pt() * scale_offset),
                Ratio::new(size.y.to_pt() * scale_offset),
            )),
        gradient: gradient.clone(),
        angle: Gradient::correct_aspect_ratio(rotation, size.aspect_ratio()),
        page_size: ctx.size,
    };

    ctx.resources.colors.mark_as_used(color_space_of(gradient));

    if gradient.is_transparent() {
        ctx.resources.colors.mark_as_used(ColorSpace::D65Gray);
    }

    ctx.resources.gradients.insert(pdf_gradient)
}

/// Writes a single Coons Patch as defined in the PDF specification
/// to a binary vec.
///
/// Structure:
///  - flag: `u8`
///  - points: `[u16; 24]`
///  - colors: `[u16; 4*N]` (N = number of components)
fn write_patch(
    target: &mut Vec<u8>,
    t: f32,
    t1: f32,
    c0: &[u16],
    c1: &[u16],
    angle: Angle,
) {
    let theta = -TAU * t + angle.to_rad() as f32 + PI;
    let theta1 = -TAU * t1 + angle.to_rad() as f32 + PI;

    let (cp1, cp2) =
        control_point(Point::new(Abs::pt(0.5), Abs::pt(0.5)), 0.5, theta, theta1);

    // Push the flag
    target.push(0);

    let p1 =
        [u16::quantize(0.5, [0.0, 1.0]).to_be(), u16::quantize(0.5, [0.0, 1.0]).to_be()];

    let p2 = [
        u16::quantize(theta.cos(), [-1.0, 1.0]).to_be(),
        u16::quantize(theta.sin(), [-1.0, 1.0]).to_be(),
    ];

    let p3 = [
        u16::quantize(theta1.cos(), [-1.0, 1.0]).to_be(),
        u16::quantize(theta1.sin(), [-1.0, 1.0]).to_be(),
    ];

    let cp1 = [
        u16::quantize(cp1.x.to_f32(), [0.0, 1.0]).to_be(),
        u16::quantize(cp1.y.to_f32(), [0.0, 1.0]).to_be(),
    ];

    let cp2 = [
        u16::quantize(cp2.x.to_f32(), [0.0, 1.0]).to_be(),
        u16::quantize(cp2.y.to_f32(), [0.0, 1.0]).to_be(),
    ];

    // Push the points
    target.extend_from_slice(bytemuck::cast_slice(&[
        p1, p1, p2, p2, cp1, cp2, p3, p3, p1, p1, p1, p1,
    ]));

    // Push the colors.
    let colors = [c0, c0, c1, c1]
        .into_iter()
        .flat_map(|c| c.iter().copied().map(u16::to_be_bytes))
        .flatten();

    target.extend(colors);
}

fn control_point(c: Point, r: f32, angle_start: f32, angle_end: f32) -> (Point, Point) {
    let n = (TAU / (angle_end - angle_start)).abs();
    let f = ((angle_end - angle_start) / n).tan() * 4.0 / 3.0;

    let p1 = c + Point::new(
        Abs::pt((r * angle_start.cos() - f * r * angle_start.sin()) as f64),
        Abs::pt((r * angle_start.sin() + f * r * angle_start.cos()) as f64),
    );

    let p2 = c + Point::new(
        Abs::pt((r * angle_end.cos() + f * r * angle_end.sin()) as f64),
        Abs::pt((r * angle_end.sin() - f * r * angle_end.cos()) as f64),
    );

    (p1, p2)
}

#[comemo::memoize]
fn compute_vertex_stream(gradient: &Gradient, aspect_ratio: Ratio) -> Arc<Vec<u8>> {
    let Gradient::Conic(conic) = gradient else { unreachable!() };

    // Generated vertices for the Coons patches
    let mut vertices = Vec::new();

    // Correct the gradient's angle
    let angle = Gradient::correct_aspect_ratio(conic.angle, aspect_ratio);

    for window in conic.stops.windows(2) {
        let ((c0, t0), (c1, t1)) = (window[0], window[1]);

        // Precision:
        // - On an even color, insert a stop every 90deg
        // - For a hue-based color space, insert 200 stops minimum
        // - On any other, insert 20 stops minimum
        let max_dt = if c0 == c1 {
            0.25
        } else if conic.space.hue_index().is_some() {
            0.005
        } else {
            0.05
        };
        let encode_space = conic
            .space
            .hue_index()
            .map(|_| ColorSpace::Oklab)
            .unwrap_or(conic.space);
        let mut t_x = t0.get();
        let dt = (t1.get() - t0.get()).min(max_dt);

        // Special casing for sharp gradients.
        if t0 == t1 {
            write_patch(
                &mut vertices,
                t0.get() as f32,
                t1.get() as f32,
                &encode_space.convert(c0),
                &encode_space.convert(c1),
                angle,
            );
            continue;
        }

        while t_x < t1.get() {
            let t_next = (t_x + dt).min(t1.get());

            // The current progress in the current window.
            let t = |t| (t - t0.get()) / (t1.get() - t0.get());
            let c = Color::mix_iter(
                [WeightedColor::new(c0, 1.0 - t(t_x)), WeightedColor::new(c1, t(t_x))],
                conic.space,
            )
            .unwrap();

            let c_next = Color::mix_iter(
                [
                    WeightedColor::new(c0, 1.0 - t(t_next)),
                    WeightedColor::new(c1, t(t_next)),
                ],
                conic.space,
            )
            .unwrap();

            write_patch(
                &mut vertices,
                t_x as f32,
                t_next as f32,
                &encode_space.convert(c),
                &encode_space.convert(c_next),
                angle,
            );

            t_x = t_next;
        }
    }

    Arc::new(deflate(&vertices))
}

fn color_space_of(gradient: &Gradient) -> ColorSpace {
    if gradient.space().hue_index().is_some() {
        ColorSpace::Oklab
    } else {
        gradient.space()
    }
}
