use core::schema::*;
use core::codec::*;
use std::io;
use std::rc::Rc;
use core::index::Index;
use core::analyzer::SimpleTokenizer;
use core::index::SerializableSegment;
use core::analyzer::StreamingIterator;
use core::index::Segment;
use core::index::SegmentInfo;
use core::postings::PostingsWriter;
use core::fastfield::U32FastFieldsWriter;
use std::sync::mpsc;
use std::sync::mpsc::channel;
use std::thread;
use std::sync::Mutex;
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::sync::Arc;

pub struct IndexWriter {
	// segment_writers: Vec<SegmentWriter>,
	threads: Vec<JoinHandle<()>>,
	index: Index,
	schema: Schema,
	queue_input: SyncSender<ArcDoc>,
}

type ArcDoc = Arc<Document>;

impl IndexWriter {

	pub fn open(index: &Index, num_threads: usize) -> io::Result<IndexWriter> {
		let schema = index.schema();
		let (queue_input, queue_output): (SyncSender<ArcDoc>, Receiver<ArcDoc>) = mpsc::sync_channel(10_000);
		let queue_output_sendable = Arc::new(Mutex::new(queue_output));
		let threads = (0..num_threads).map(|thread_id|  {
			let queue_output_clone = queue_output_sendable.clone();
			let index_clone = index.clone();
			let schema_clone = schema.clone();
			thread::spawn(move || {
				let mut docs_remaining = true;
				while docs_remaining {
					let segment = index_clone.new_segment();
					let mut segment_writer = SegmentWriter::for_segment(segment, &schema_clone).unwrap();
					println!("thread_id {}", thread_id);
					for i in 0..500 {
						let doc: ArcDoc;
						{
							let queue = queue_output_clone.lock().unwrap();
							match queue.recv() {
								Ok(doc_) => {
									doc = doc_;
								}
								Err(_) => {
									docs_remaining = false;
									println!("err");
									break;
								}
							}
						}
						// TODO stop unwrapping that one.
						segment_writer.add_document(&*doc, &schema_clone).unwrap();
					}
					println!("finalize {}", thread_id);
					segment_writer.finalize().unwrap();
					// try!(self.directory.sync(segment.clone()));
					// try!(self.directory.publish_segment(segment.clone()));
					// segment_writer.commit().unwrap();
				}
			})
		}).collect();
		// let segment = directory.new_segment();
		// let segment_writer = try!(SegmentWriter::for_segment(segment, &schema));
		// segment_writers.push(segment_writer);
		Ok(IndexWriter {
			threads: threads,
			// segment_writers: segment_writers, //Rc::new(segment_writer),
			index: index.clone(),
			schema: schema,
			queue_input: queue_input,
		})
	}

	pub fn wait(self,) {
		for thread in self.threads {
			thread.join();
		}
	}

    pub fn add_document(&mut self, doc: Document) -> io::Result<()> {
        // Rc::get_mut(&mut self.segment_writer).unwrap().add_document(&doc, &self.schema)
		let arc_doc = ArcDoc::new(doc);
		self.queue_input.send(arc_doc);
		Ok(())
    }

    pub fn commit(&mut self,) -> io::Result<Vec<Segment>> {
		/*
		let segment_writer_rc = self.segment_writer.clone();
		let segment = self.directory.new_segment();
		self.segment_writer = Rc::new(try!(SegmentWriter::for_segment(segment, &self.schema)));
		match Rc::try_unwrap(segment_writer_rc) {
			Ok(segment_writer) => {
				let segment = segment_writer.segment();
				try!(segment_writer.finalize());
				try!(self.directory.sync(segment.clone()));
				try!(self.directory.publish_segment(segment.clone()));
				Ok(segment)
			},
			Err(_) => {
				panic!("error while acquiring segment writer.");
			}
		}
		*/
		Ok(Vec::new())
	}

}


pub struct SegmentWriter {
    max_doc: DocId,
	tokenizer: SimpleTokenizer,
	postings_writer: PostingsWriter,
	segment_serializer: SegmentSerializer,
	fast_field_writers: U32FastFieldsWriter,
}

impl SegmentWriter {
	// Write on disk all of the stuff that
	// is still on RAM :
	// - the dictionary in an fst
	// - the postings
	// - the segment info
	fn finalize(mut self,) -> io::Result<()> {
		try!(self.postings_writer.serialize(self.segment_serializer.get_postings_serializer()));
		{
			let segment_info = SegmentInfo {
				max_doc: self.max_doc
			};
			try!(self.segment_serializer.write_segment_info(&segment_info));
		}
		self.segment_serializer.close()
	}

	pub fn segment(&self,) -> Segment {
		self.segment_serializer.segment()
	}

	fn for_segment(segment: Segment, schema: &Schema) -> io::Result<SegmentWriter> {
		let segment_serializer = try!(SegmentSerializer::for_segment(&segment));
		Ok(SegmentWriter {
			max_doc: 0,
			postings_writer: PostingsWriter::new(),
			segment_serializer: segment_serializer,
			tokenizer: SimpleTokenizer::new(),
			fast_field_writers: U32FastFieldsWriter::from_schema(schema),
		})
	}

    pub fn add_document(&mut self, doc: &Document, schema: &Schema) -> io::Result<()> {
        let doc_id = self.max_doc;
        for field_value in doc.text_fields() {
			let field_options = schema.text_field_options(&field_value.field);
			if field_options.is_tokenized_indexed() {
				let mut tokens = self.tokenizer.tokenize(&field_value.text);
				loop {
					match tokens.next() {
						Some(token) => {
							let term = Term::from_field_text(&field_value.field, token);
							self.postings_writer.suscribe(doc_id, term);
						},
						None => { break; }
					}
				}
			}
		}
		for field_value in doc.u32_fields() {
            let field_options = schema.u32_field_options(&field_value.field);
            if field_options.is_indexed() {
                let term = Term::from_field_u32(&field_value.field, field_value.value);
                self.postings_writer.suscribe(doc_id, term);
            }
		}
		let mut stored_fieldvalues_it = doc.text_fields().filter(|text_field_value| {
			schema.text_field_options(&text_field_value.field).is_stored()
		});
		self.fast_field_writers.add_document(&doc);
		try!(self.segment_serializer.store_doc(&mut stored_fieldvalues_it));
        self.max_doc += 1;
		Ok(())
    }

}

impl SerializableSegment for SegmentWriter {
	fn write(&self, mut serializer: SegmentSerializer) -> io::Result<()> {
		try!(self.postings_writer.serialize(serializer.get_postings_serializer()));
		try!(self.fast_field_writers.serialize(serializer.get_fast_field_serializer()));
		try!(serializer.close());
		Ok(())
	}
}
