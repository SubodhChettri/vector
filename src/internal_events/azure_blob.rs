use super::prelude::{error_stage, error_type};
use metrics::counter;
use uuid::Uuid;
use vector_core::internal_event::InternalEvent;

#[derive(Debug)]
pub struct AzureBlobResponseError {
    error_code: String,
}

impl From<hyper::StatusCode> for AzureBlobResponseError {
    fn from(code: hyper::StatusCode) -> Self {
        Self {
            error_code: super::prelude::http_error_code(code.as_u16()),
        }
    }
}

impl InternalEvent for AzureBlobResponseError {
    fn emit(self) {
        error!(
            message = "HTTP error response.",
            error_code = %self.error_code,
            error_type = error_type::REQUEST_FAILED,
            stage = error_stage::SENDING,
        );
        counter!(
            "component_errors_total", 1,
            "error_code" => self.error_code,
            "error_type" => error_type::REQUEST_FAILED,
            "stage" => error_stage::SENDING,
        );
        // deprecated
        counter!("http_error_response_total", 1);
    }
}

#[derive(Debug)]
pub struct AzureBlobHttpError {
    pub error: String,
}

impl InternalEvent for AzureBlobHttpError {
    fn emit(self) {
        error!(
            message = "Error processing request.",
            error = %self.error,
            error_type = error_type::REQUEST_FAILED,
            stage = error_stage::SENDING,
            internal_log_rate_secs = 10
        );
        counter!(
            "component_errors_total", 1,
            "error_type" => error_type::REQUEST_FAILED,
            "stage" => error_stage::SENDING,
        );
        // deprecated
        counter!("http_request_errors_total", 1);
    }
}

pub struct AzureBlobEventsSent {
    pub request_id: Uuid,
    pub byte_size: usize,
}

impl InternalEvent for AzureBlobEventsSent {
    fn emit(self) {
        trace!(message = "Events sent.", request_id = %self.request_id, count = 1, byte_size = %self.byte_size);
        counter!("component_sent_events_total", 1);
        counter!("component_sent_event_bytes_total", self.byte_size as u64);
    }
}
