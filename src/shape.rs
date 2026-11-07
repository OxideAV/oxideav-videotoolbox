//! Stream-level output description shared by every VideoToolbox decoder.
//!
//! The core decoder contract keeps the picture shape (size, pixel
//! layout) out of the frames: [`Decoder::output_params`] states it once,
//! and a mid-stream change is announced by one
//! [`Frame::StreamParams`] event returned immediately before the first
//! frame of the new shape, together with one bump of
//! [`Decoder::output_params_generation`].
//!
//! Every decoded picture carries the shape it was actually delivered in
//! (the `CVPixelBuffer` geometry and the label the planes were brought
//! to), so the announcement is made where the frames leave the decoder:
//! pictures of the old shape still queued come out first, and the event
//! sits exactly at the boundary whatever re-ordering, session
//! re-creation, `flush` drain or `reset` happened upstream. Before the
//! first frame the description is a prediction from the stream's own
//! headers (parameter sets, configuration record, container claim);
//! the first frame settles it without an event.
//!
//! [`Decoder::output_params`]: oxideav_core::Decoder::output_params
//! [`Decoder::output_params_generation`]: oxideav_core::Decoder::output_params_generation
//! [`Frame::StreamParams`]: oxideav_core::Frame::StreamParams

use oxideav_core::{CodecId, CodecParameters, PixelFormat};

/// Geometry and layout of the planes of one delivered picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
}

/// Output-description bookkeeping (see the module docs).
#[derive(Debug)]
pub struct ShapeTracker {
    codec_id: CodecId,
    /// Description stated before the first frame (may be partial).
    predicted: Option<CodecParameters>,
    /// Shape of the frame most recently returned by `receive_frame`.
    published: Option<Shape>,
    generation: u64,
}

impl ShapeTracker {
    pub fn new(codec_id: CodecId) -> Self {
        ShapeTracker {
            codec_id,
            predicted: None,
            published: None,
            generation: 0,
        }
    }

    /// Record what the stream headers say the frames will look like.
    /// Only consulted until the first frame has been returned (after
    /// that the description follows the frames).
    pub fn predict(
        &mut self,
        width: Option<u32>,
        height: Option<u32>,
        pixel_format: Option<PixelFormat>,
    ) {
        if self.published.is_some() {
            return;
        }
        if width.is_none() && height.is_none() && pixel_format.is_none() {
            self.predicted = None;
            return;
        }
        let mut p = CodecParameters::video(self.codec_id.clone());
        p.width = width;
        p.height = height;
        p.pixel_format = pixel_format;
        self.predicted = Some(p);
    }

    /// Called with the shape of the frame about to be returned. Returns
    /// `true` when that shape differs from the one last returned — the
    /// caller then returns a `Frame::StreamParams` event carrying
    /// [`params`](Self::params) and keeps the frame for the next call.
    /// The generation has moved when this returns `true`; a second call
    /// with the same shape returns `false`, so one change yields
    /// exactly one event.
    pub fn announce(&mut self, shape: Shape) -> bool {
        match self.published {
            Some(p) if p == shape => false,
            None => {
                self.published = Some(shape);
                false
            }
            Some(_) => {
                self.published = Some(shape);
                self.generation = self.generation.wrapping_add(1);
                true
            }
        }
    }

    /// The current output description: the shape of the frame most
    /// recently returned, else the prediction.
    pub fn params(&self) -> Option<CodecParameters> {
        match self.published {
            Some(s) => {
                let mut p = CodecParameters::video(self.codec_id.clone());
                p.width = Some(s.width);
                p.height = Some(s.height);
                p.pixel_format = Some(s.pixel_format);
                Some(p)
            }
            None => self.predicted.clone(),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(w: u32, h: u32, pf: PixelFormat) -> Shape {
        Shape {
            width: w,
            height: h,
            pixel_format: pf,
        }
    }

    #[test]
    fn first_shape_is_silent_changes_are_announced_once() {
        let mut t = ShapeTracker::new(CodecId::new("h264"));
        assert!(t.params().is_none());
        t.predict(Some(64), Some(48), Some(PixelFormat::Yuv420P));
        assert_eq!(t.params().unwrap().width, Some(64));
        // A refined prediction before the first frame is not a change.
        t.predict(Some(200), Some(120), Some(PixelFormat::Yuv420P));
        assert_eq!(t.params().unwrap().width, Some(200));
        assert!(!t.announce(s(200, 120, PixelFormat::Yuv420P)));
        assert!(!t.announce(s(200, 120, PixelFormat::Yuv420P)));
        assert_eq!(t.generation(), 0);
        // Predictions after the first frame are ignored.
        t.predict(Some(1), Some(1), None);
        assert_eq!(t.params().unwrap().width, Some(200));

        assert!(t.announce(s(64, 48, PixelFormat::Yuv420P)));
        assert_eq!(t.generation(), 1);
        let p = t.params().unwrap();
        assert_eq!((p.width, p.height), (Some(64), Some(48)));
        assert!(!t.announce(s(64, 48, PixelFormat::Yuv420P)));
        assert!(t.announce(s(64, 48, PixelFormat::Yuv420P10Le)));
        assert_eq!(t.generation(), 2);
        assert_eq!(
            t.params().unwrap().pixel_format,
            Some(PixelFormat::Yuv420P10Le)
        );
    }
}
