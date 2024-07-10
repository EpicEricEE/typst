use std::collections::HashMap;

use pdf_writer::{types::MaskType, Content, Finish, Name, Rect, Ref};
use typst::layout::Transform;

use crate::gradient::{shading, PdfGradient};
use crate::{transform_to_array, PdfChunk, WithGlobalRefs};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SoftMask {
    /// The transform to apply to the gradient.
    pub transform: Transform,
    /// The gradient to use for the soft mask.
    pub gradient: PdfGradient,
}

/// A PDF external graphics state.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ExtGState {
    /// In the range 0-255, needs to be divided before being written into the graphics state!
    pub stroke_opacity: u8,
    /// In the range 0-255, needs to be divided before being written into the graphics state!
    pub fill_opacity: u8,
    /// The soft mask to use for this graphics state.
    pub soft_mask: Option<SoftMask>,
}

impl Default for ExtGState {
    fn default() -> Self {
        Self {
            stroke_opacity: 255,
            fill_opacity: 255,
            soft_mask: None,
        }
    }
}

impl ExtGState {
    pub fn uses_opacities(&self) -> bool {
        self.stroke_opacity != 255 || self.fill_opacity != 255
    }
}

/// Embed all used external graphics states into the PDF.
pub fn write_graphic_states(
    context: &WithGlobalRefs,
) -> (PdfChunk, HashMap<ExtGState, Ref>) {
    let mut chunk = PdfChunk::new();
    let mut out = HashMap::new();
    context.resources.traverse(&mut |resources| {
        for external_gs in resources.ext_gs.items() {
            if out.contains_key(external_gs) {
                continue;
            }

            let id = chunk.alloc();
            out.insert(external_gs.clone(), id);

            let soft_mask_group = external_gs.soft_mask.as_ref().map(|soft_mask| {
                const SHADING_NAME: Name = Name(b"ShX");
                let shading = shading(
                    context,
                    &soft_mask.gradient,
                    &mut chunk,
                    soft_mask.gradient.gradient.space(),
                );

                let group = chunk.alloc();

                let mut content = Content::new();
                content.shading(SHADING_NAME);

                let data = content.finish();
                let mut xobject = chunk.form_xobject(group, &data);
                xobject
                    .matrix(transform_to_array(soft_mask.transform))
                    .bbox(Rect::new(0.0, 0.0, 1.0, 1.0));
                xobject.group().transparency().color_space().d65_gray();
                xobject.resources().shadings().pair(SHADING_NAME, shading).finish();

                group
            });

            let mut extgstate = chunk.ext_graphics(id);
            extgstate
                .non_stroking_alpha(external_gs.fill_opacity as f32 / 255.0)
                .stroking_alpha(external_gs.stroke_opacity as f32 / 255.0);

            if let Some(soft_mask_group) = soft_mask_group {
                extgstate
                    .soft_mask()
                    .subtype(MaskType::Luminosity)
                    .group(soft_mask_group)
                    .finish();
            } else {
                extgstate.soft_mask_name(Name(b"None"));
            }
        }
    });

    (chunk, out)
}
