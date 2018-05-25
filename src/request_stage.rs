//! The `request_stage` processes thin client Request messages.

use bincode::{deserialize, serialize};
use packet;
use packet::SharedPackets;
use rayon::prelude::*;
use request::Request;
use request_processor::RequestProcessor;
use result::Result;
use serde::Serialize;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::thread::{spawn, JoinHandle};
use std::time::Instant;
use streamer;
use timing;

pub struct RequestStage {
    pub thread_hdl: JoinHandle<()>,
    pub blob_receiver: streamer::BlobReceiver,
    pub request_processor: Arc<RequestProcessor>,
}

impl RequestStage {
    pub fn deserialize_requests(p: &packet::Packets) -> Vec<Option<(Request, SocketAddr)>> {
        p.packets
            .par_iter()
            .map(|x| {
                deserialize(&x.data[0..x.meta.size])
                    .map(|req| (req, x.meta.addr()))
                    .ok()
            })
            .collect()
    }

    /// Split Request list into verified transactions and the rest
    fn serialize_response<T: Serialize>(
        resp: T,
        rsp_addr: SocketAddr,
        blob_recycler: &packet::BlobRecycler,
    ) -> Result<packet::SharedBlob> {
        let blob = blob_recycler.allocate();
        {
            let mut b = blob.write().unwrap();
            let v = serialize(&resp)?;
            let len = v.len();
            b.data[..len].copy_from_slice(&v);
            b.meta.size = len;
            b.meta.set_addr(&rsp_addr);
        }
        Ok(blob)
    }

    fn serialize_responses<T: Serialize>(
        rsps: Vec<(T, SocketAddr)>,
        blob_recycler: &packet::BlobRecycler,
    ) -> Result<VecDeque<packet::SharedBlob>> {
        let mut blobs = VecDeque::new();
        for (resp, rsp_addr) in rsps {
            blobs.push_back(Self::serialize_response(resp, rsp_addr, blob_recycler)?);
        }
        Ok(blobs)
    }

    pub fn process_request_packets(
        request_processor: &RequestProcessor,
        packet_receiver: &Receiver<SharedPackets>,
        blob_sender: &streamer::BlobSender,
        packet_recycler: &packet::PacketRecycler,
        blob_recycler: &packet::BlobRecycler,
    ) -> Result<()> {
        let (batch, batch_len) = streamer::recv_batch(packet_receiver)?;

        info!(
            "@{:?} request_stage: processing: {}",
            timing::timestamp(),
            batch_len
        );

        let mut reqs_len = 0;
        let proc_start = Instant::now();
        for msgs in batch {
            let reqs: Vec<_> = Self::deserialize_requests(&msgs.read().unwrap())
                .into_iter()
                .filter_map(|x| x)
                .collect();
            reqs_len += reqs.len();

            let rsps = request_processor.process_requests(reqs);

            let blobs = Self::serialize_responses(rsps, blob_recycler)?;
            if !blobs.is_empty() {
                info!("process: sending blobs: {}", blobs.len());
                //don't wake up the other side if there is nothing
                blob_sender.send(blobs)?;
            }
            packet_recycler.recycle(msgs);
        }
        let total_time_s = timing::duration_as_s(&proc_start.elapsed());
        let total_time_ms = timing::duration_as_ms(&proc_start.elapsed());
        info!(
            "@{:?} done process batches: {} time: {:?}ms reqs: {} reqs/s: {}",
            timing::timestamp(),
            batch_len,
            total_time_ms,
            reqs_len,
            (reqs_len as f32) / (total_time_s)
        );
        Ok(())
    }
    pub fn new(
        request_processor: RequestProcessor,
        exit: Arc<AtomicBool>,
        packet_receiver: Receiver<SharedPackets>,
        packet_recycler: packet::PacketRecycler,
        blob_recycler: packet::BlobRecycler,
    ) -> Self {
        let request_processor = Arc::new(request_processor);
        let request_processor_ = request_processor.clone();
        let (blob_sender, blob_receiver) = channel();
        let thread_hdl = spawn(move || loop {
            let e = Self::process_request_packets(
                &request_processor_,
                &packet_receiver,
                &blob_sender,
                &packet_recycler,
                &blob_recycler,
            );
            if e.is_err() {
                if exit.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
        RequestStage {
            thread_hdl,
            blob_receiver,
            request_processor,
        }
    }
}
