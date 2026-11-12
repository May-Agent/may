//! Stamps every outgoing RTP packet with the time it is sent (the
//! `abs-send-time` header extension).
//!
//! Without it a receiver's delay-based bandwidth estimate (REMB) times each
//! frame by its RTP timestamp — when the frame reached the encoder — so any
//! change in how long frames take from capture to the socket reads as
//! network queueing, and the estimate falls on a link that is not congested.

use std::sync::Arc;
use std::time::SystemTime;

use webrtc::interceptor::stream_info::StreamInfo;
use webrtc::interceptor::{
    Attributes, Error, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader,
    RTPWriter,
};
use webrtc::rtp::extension::abs_send_time_extension::AbsSendTimeExtension;
use webrtc::rtp::packet::Packet;
use webrtc::util::Marshal;

pub const ABS_SEND_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time";

/// Registers [`SendTime`] on an interceptor registry.
pub struct SendTimeBuilder;

impl InterceptorBuilder for SendTimeBuilder {
    fn build(&self, _id: &str) -> Result<Arc<dyn Interceptor + Send + Sync>, Error> {
        Ok(Arc::new(SendTime))
    }
}

/// Stamps the local streams that negotiated `abs-send-time`.
struct SendTime;

#[async_trait::async_trait]
impl Interceptor for SendTime {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        reader
    }

    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        writer
    }

    async fn bind_local_stream(
        &self,
        info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        // A viewer that did not offer the extension gets the packets as they are.
        let Some(id) = info
            .rtp_header_extensions
            .iter()
            .find(|extension| extension.uri == ABS_SEND_TIME_URI)
            .and_then(|extension| u8::try_from(extension.id).ok())
        else {
            return writer;
        };
        Arc::new(StampedWriter { next: writer, id })
    }

    async fn unbind_local_stream(&self, _info: &StreamInfo) {}

    async fn bind_remote_stream(
        &self,
        _info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        reader
    }

    async fn unbind_remote_stream(&self, _info: &StreamInfo) {}

    async fn close(&self) -> Result<(), Error> {
        Ok(())
    }
}

struct StampedWriter {
    next: Arc<dyn RTPWriter + Send + Sync>,
    id: u8,
}

#[async_trait::async_trait]
impl RTPWriter for StampedWriter {
    async fn write(&self, packet: &Packet, attributes: &Attributes) -> Result<usize, Error> {
        let stamp = AbsSendTimeExtension::new(SystemTime::now()).marshal()?;
        let mut packet = packet.clone();
        packet.header.set_extension(self.id, stamp)?;
        self.next.write(&packet, attributes).await
    }
}
