use crate::elasticsearch::{BulkRequestCommand, BulkRequestError, Elasticsearch};
use pgx::*;
use serde_json::{json, Value};
use std::any::Any;
use std::collections::HashMap;
use std::io::{Error, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

const BULK_FILTER_PATH: &str = "errors,items.*.error";

pub(crate) struct Handler {
    threads: Vec<JoinHandle<usize>>,
    active_thread_cnt: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    total_docs: usize,
    elasticsearch: Elasticsearch,
    concurrency: usize,
    bulk_sender: crossbeam::channel::Sender<BulkRequestCommand>,
    bulk_receiver: crossbeam::channel::Receiver<BulkRequestCommand>,
    _error_sender: crossbeam::channel::Sender<BulkRequestError>,
}

struct BulkReceiver {
    first: Option<BulkRequestCommand>,
    in_flight: Arc<AtomicUsize>,
    receiver: crossbeam::channel::Receiver<BulkRequestCommand>,
    bytes_out: usize,
    docs_out: Arc<AtomicUsize>,
    buffer: Vec<u8>,
}

impl std::io::Read for BulkReceiver {
    fn read(&mut self, mut buf: &mut [u8]) -> Result<usize, Error> {
        // if we have a first value, we need to send it out first
        if let Some(command) = self.first.take() {
            self.serialize_command(command);
        }

        // otherwise we'll wait to receive a command
        if self.docs_out.load(Ordering::SeqCst) < 10_000 && self.bytes_out < 8 * 1024 * 1024 {
            // but only if we haven't exceeded the max _bulk docs limit
            match self.receiver.recv_timeout(Duration::from_millis(333)) {
                Ok(command) => self.serialize_command(command),
                Err(_) => {}
            }
        }

        let amt = buf.write(&self.buffer)?;
        if amt > 0 {
            // move our bytes forward the amount we wrote above
            let (_, right) = self.buffer.split_at(amt);
            self.buffer = Vec::from(right);
            self.bytes_out += amt;
        }

        Ok(amt)
    }
}

impl BulkReceiver {
    fn serialize_command(&mut self, command: BulkRequestCommand) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.docs_out.fetch_add(1, Ordering::SeqCst);

        // build json of this entire command and store in self.bytes
        match command {
            BulkRequestCommand::Insert {
                ctid,
                cmin: _,
                cmax: _,
                xmin: _,
                xmax: _,
                builder: doc,
            } => {
                serde_json::to_writer(
                    &mut self.buffer,
                    &json! {
                        {"index": {"_id": item_pointer_to_u64(ctid) } }
                    },
                )
                .expect("failed to serialize index line");
                self.buffer.push(b'\n');

                let doc_as_json = doc.build();
                self.buffer.append(&mut doc_as_json.into_bytes());
                self.buffer.push(b'\n');
            }
            BulkRequestCommand::Update { .. } => panic!("unsupported"),
            BulkRequestCommand::DeleteByXmin { .. } => panic!("unsupported"),
            BulkRequestCommand::DeleteByXmax { .. } => panic!("unsupported"),
            BulkRequestCommand::Interrupt => panic!("unsupported"),
            BulkRequestCommand::Done => panic!("unsupported"),
        }
    }
}

impl From<BulkReceiver> for reqwest::Body {
    fn from(reader: BulkReceiver) -> Self {
        reqwest::Body::new(reader)
    }
}

impl Handler {
    pub(crate) fn new(
        elasticsearch: Elasticsearch,
        concurrency: usize,
        _error_sender: crossbeam::channel::Sender<BulkRequestError>,
    ) -> Self {
        let (tx, rx) = crossbeam::channel::bounded(10_000);

        Handler {
            threads: Vec::new(),
            active_thread_cnt: Arc::new(AtomicUsize::new(0)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            total_docs: 0,
            elasticsearch,
            concurrency,
            bulk_sender: tx,
            bulk_receiver: rx,
            _error_sender,
        }
    }

    pub fn queue_command(
        &mut self,
        command: BulkRequestCommand,
    ) -> Result<(), crossbeam::SendError<BulkRequestCommand>> {
        if self.total_docs % 10000 == 0 {
            info!(
                "total={}, in_flight={}, active_threads={}",
                self.total_docs,
                self.in_flight.load(Ordering::SeqCst),
                self.active_thread_cnt.load(Ordering::SeqCst)
            );
        }

        self.total_docs += 1;
        let nthreads = self.active_thread_cnt.load(Ordering::SeqCst);
        let queue_len = self.bulk_sender.len();
        let capacity = self.bulk_sender.capacity().unwrap();

        if nthreads == 0 || (queue_len > capacity / self.concurrency && nthreads < self.concurrency)
        {
            self.threads
                .push(self.create_thread(self.threads.len(), Some(command)));

            Ok(())
        } else {
            self.bulk_sender.send(command)
        }
    }

    fn create_thread(
        &self,
        thread_id: usize,
        mut initial_command: Option<BulkRequestCommand>,
    ) -> JoinHandle<usize> {
        let es = self.elasticsearch.clone();
        let rx = self.bulk_receiver.clone();
        let in_flight = self.in_flight.clone();
        let active_thread_cnt = self.active_thread_cnt.clone();

        info!("spawning thread #{}", thread_id + 1);
        self.active_thread_cnt.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let mut total_docs_out = 0;
            loop {
                let initial_command = initial_command.take();
                let first;

                if initial_command.is_some() {
                    first = initial_command;
                } else {
                    first = Some(match rx.recv() {
                        Ok(command) => command,
                        Err(_) => {
                            // we don't have a first command to deal with on this iteration b/c
                            // the channel has been shutdown.  we're simply out of records
                            // and can safely break out
                            eprintln!("thread #{} exiting b/c channel is closed", thread_id);
                            break;
                        }
                    })
                }

                let docs_out = Arc::new(AtomicUsize::new(0));
                let rx = rx.clone();
                let reader = BulkReceiver {
                    first,
                    in_flight: in_flight.clone(),
                    receiver: rx.clone(),
                    bytes_out: 0,
                    docs_out: docs_out.clone(),
                    buffer: Vec::new(),
                };

                let url = &format!(
                    "{}{}/_bulk?filter_path={}",
                    es.url, es.index_name, BULK_FILTER_PATH
                );
                let client = reqwest::Client::new();
                let response = client
                    .post(url)
                    .header("content-type", "application/json")
                    .body(reader)
                    .send();

                let docs_out = docs_out.load(Ordering::SeqCst);

                in_flight.fetch_sub(docs_out, Ordering::SeqCst);

                total_docs_out += docs_out;

                match response {
                    // we got a valid response from ES
                    Ok(mut response) => {
                        // quick check on the status code
                        let code = response.status().as_u16();
                        if code < 200 || (code >= 300 && code != 404) {
                            let mut resp_string = String::new();
                            response
                                .read_to_string(&mut resp_string)
                                .expect("unable to convert HTTP response to a string");
                            panic!("{}", resp_string)
                        } else if code != 200 {
                            let mut resp_string = String::new();
                            response
                                .read_to_string(&mut resp_string)
                                .expect("unable to convert HTTP response to a string");

                            match serde_json::from_str::<HashMap<String, Value>>(&resp_string) {
                                // got a valid json response
                                Ok(response) => {
                                    // does it contain a general error?
                                    match *response.get("error").unwrap_or(&Value::Bool(false)) {
                                        Value::Bool(b) if b == false => { /* we're all good */ }
                                        _ => panic!("{}", resp_string),
                                    }

                                    // does it contain errors related to the docs we indexed?
                                    match *response.get("errors").unwrap_or(&Value::Bool(false)) {
                                        Value::Bool(b) if b == false => { /* we're all good */ }
                                        _ => panic!("{}", resp_string),
                                    }
                                }

                                // got a response that wasn't json, so just panic with it
                                Err(_) => panic!("{}", resp_string),
                            }
                        }
                    }

                    // this is likely a general reqwest/network communication error
                    Err(e) => panic!("{:?}", e),
                }

                if docs_out == 0 {
                    eprintln!("thread #{} exiting b/x docs_out == 0", thread_id);
                    break;
                }
            }

            active_thread_cnt.fetch_sub(1, Ordering::SeqCst);
            total_docs_out
        })
    }

    pub(crate) fn wait_for_completion(self) -> Result<usize, BulkRequestError> {
        // drop the sender side of the channel since we're done
        // this will signal the receivers that once their queues are empty
        // there's nothing left for them to do
        std::mem::drop(self.bulk_sender);

        info!("thead count={}", self.threads.len());
        let mut cnt = 0;
        for (i, jh) in self.threads.into_iter().enumerate() {
            match jh.join() {
                Ok(many) => {
                    info!(
                        "thread #{}: total_docs_out={}, in_flight={}",
                        i + 1,
                        many,
                        self.in_flight.load(Ordering::SeqCst)
                    );
                    cnt += many;
                }
                Err(e) => panic!("Got an error joining on a thread: {}", downcast_err(e)),
            }
        }

        Ok(cnt)
    }

    pub(crate) fn terminate(&mut self) {}
}

fn downcast_err(e: Box<dyn Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.to_string()
    } else {
        // not a type we understand, so use a generic string
        "Box<Any>".to_string()
    }
}