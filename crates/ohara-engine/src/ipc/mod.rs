pub mod envelope;
pub mod frame;
pub use envelope::{ErrorCode, ErrorPayload, Request, RequestMethod, Response};
pub use frame::{read_frame, read_frame_or_eof, write_frame};
