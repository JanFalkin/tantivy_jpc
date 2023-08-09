use crate::debug;
use crate::make_internal_json_error;
use crate::ErrorKinds;
use crate::InternalCallResult;
use crate::TantivySession;
use tantivy::DocAddress;
use tantivy::Searcher;
use tantivy::TERMINATED;

extern crate serde;
extern crate serde_derive;
extern crate serde_json;
use crate::HashMap;
use log::error;
use serde_derive::{Deserialize, Serialize};
use std::fmt::Write;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::Query;
use tantivy::schema::NamedFieldDocument;
use tantivy::SnippetGenerator;
use tantivy::{Document, Index};

#[derive(Serialize, Deserialize, Debug)]
pub struct ResultElement {
    pub doc: NamedFieldDocument,
    pub score: f32,
    pub explain: String,
    pub snippet_html: Option<HashMap<u64, String>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RawElement {
    pub title: String,
    pub body: String,
}

#[derive(Serialize, Deserialize)]
pub struct ResultElementDoc {
    pub doc: Document,
    pub score: f32,
}

impl TantivySession {
    pub fn handle_fuzzy_searcher(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> InternalCallResult<u32> {
        debug!("FuzzySearcher");
        if method != "fuzzy_searcher" {
            return Err(ErrorKinds::NotExist(format!(
                "expecting method fuzzy_searcher found {method}"
            )));
        }
        const DEF_LIMIT: u64 = 2;
        let top_limit = match params.as_object() {
            Some(p) => p
                .get("top_limit")
                .and_then(|u| u.as_u64())
                .unwrap_or(DEF_LIMIT),
            None => DEF_LIMIT,
        };
        let query = match self.fuzzy_q.as_deref() {
            Some(dq) => dq,
            None => {
                return make_internal_json_error(ErrorKinds::NotExist(
                    "dyn query not created".to_string(),
                ));
            }
        };
        let idx = match &self.index {
            Some(r) => r,
            None => {
                return make_internal_json_error(ErrorKinds::NotExist(
                    "Reader unavliable".to_string(),
                ))
            }
        };

        let rdr = idx.reader()?;
        let searcher = rdr.searcher();
        let td = match searcher.search(query, &(TopDocs::with_limit(top_limit as usize), Count)) {
            Ok(td) => td,
            Err(e) => {
                return make_internal_json_error(ErrorKinds::Search(format!("tantivy error = {e}")))
            }
        };
        debug!("search complete len = {}, td = {:?}", td.0.len(), td);
        let mut vret = Vec::<ResultElementDoc>::new();
        for (score, doc_address) in td.0 {
            let retrieved_doc = searcher.doc(doc_address)?;
            vret.push(ResultElementDoc {
                doc: retrieved_doc,
                score,
            });
        }
        let mut s = "".to_string();
        match writeln!(s, "{}", serde_json::to_string(&vret)?) {
            Ok(_) => {}
            Err(_) => {
                return make_internal_json_error(ErrorKinds::NotExist(
                    "format write to string failed".to_string(),
                ))
            }
        };
        self.return_buffer = s;

        self.fuzzy_q = Some(Box::new(query.clone()));
        if self.return_buffer.is_empty() {
            self.return_buffer = r#"{ "result" : "EMPTY"}"#.to_string();
        }
        Ok(0)
    }

    fn setup_searcher(&self) -> InternalCallResult<(&dyn Query, &Index, Searcher)> {
        let query = match self.dyn_q.as_ref() {
            Some(dq) => dq,
            None => {
                return make_internal_json_error(ErrorKinds::NotExist(
                    "dyn query not created".to_string(),
                ));
            }
        };
        let idx = match &self.index {
            Some(r) => r,
            None => {
                return make_internal_json_error(ErrorKinds::NotExist(
                    "Reader unavailable".to_string(),
                ))
            }
        };

        let rdr = idx.reader()?;
        let searcher = rdr.searcher();
        Ok((query, idx, searcher))
    }

    fn do_search_execute(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        idx: &Index,
        offset: usize,
        top_limit: u64,
        score: bool,
    ) -> Result<Vec<(f32, DocAddress)>, ErrorKinds> {
        let enable_scoring = match score {
            false => tantivy::query::EnableScoring::disabled_from_searcher(searcher),
            true => tantivy::query::EnableScoring::enabled_from_searcher(searcher),
        };

        match searcher.search_with_executor(
            query,
            &TopDocs::with_limit(top_limit as usize).and_offset(offset),
            idx.search_executor(),
            enable_scoring,
        ) {
            Ok(td) => Ok(td),
            Err(e) => make_internal_json_error(ErrorKinds::Search(format!(
                "do_search_execute tantivy error = {e}"
            ))),
        }
    }

    fn do_docset(&mut self, params: serde_json::Value) -> InternalCallResult<u32> {
        const DEF_LIMIT: u64 = 10;
        let (top_limit, offset, score) = match params.as_object() {
            Some(p) => (
                p.get("top_limit")
                    .and_then(|u| u.as_u64())
                    .unwrap_or(DEF_LIMIT),
                p.get("offset").and_then(|u| u.as_u64()).unwrap_or(0) as usize,
                p.get("scoring").and_then(|u| u.as_bool()).unwrap_or(true),
            ),
            None => (DEF_LIMIT, 0, true),
        };
        let (query, idx, searcher) = self.setup_searcher()?;

        let td = self.do_search_execute(&searcher, query, idx, offset, top_limit, score)?;
        debug!("search complete len = {}, td = {:?}", td.len(), td);
        let vec_str = td
            .iter()
            .map(|(score, doc_address)| {
                format!(
                    r#"{{ "score":{},   "segment_ord":{}, "doc_id":{}  }}"#,
                    score, doc_address.segment_ord, doc_address.doc_id
                )
            })
            .collect::<Vec<String>>()
            .join(", ");
        self.return_buffer = format!(r#"{{ "docset" : [{vec_str}] }}"#);
        debug!("ret = {}", self.return_buffer);
        Ok(0)
    }

    fn make_snippet(
        &self,
        v: &i64,
        searcher: &Searcher,
        query: &dyn Query,
        retrieved_doc: &Document,
    ) -> Result<String, ErrorKinds> {
        let snip_field = tantivy::schema::Field::from_field_id(*v as u32);
        let snippet_generator = SnippetGenerator::create(searcher, query, snip_field)?;
        Ok(snippet_generator.snippet_from_doc(retrieved_doc).to_html())
    }

    fn do_get_document(&mut self, params: serde_json::Value) -> InternalCallResult<u32> {
        let (segment_ord, doc_id, score, explain, fields) = match params.as_object() {
            Some(p) => (
                (p.get("segment_ord").and_then(|u| u.as_u64()).unwrap_or(0)) as u32,
                (p.get("doc_id").and_then(|u| u.as_u64()).unwrap_or(0)) as u32,
                (p.get("score").and_then(|u| u.as_f64()).unwrap_or(0.0)),
                p.get("explain").and_then(|u| u.as_bool()).unwrap_or(false),
                p.get("snippet_field")
                    .and_then(|u| u.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.as_i64())
                            .collect::<Vec<i64>>()
                    })
                    .unwrap_or_else(|| vec![]), // Default to an empty vector
            ),
            None => (0, 0, 0.0, false, vec![]), // Default values
        };

        let doc_address = DocAddress {
            doc_id,
            segment_ord,
        };
        let (query, _idx, searcher) = self.setup_searcher()?;

        let retrieved_doc = searcher.doc(doc_address)?;
        let schema = self
            .schema
            .as_ref()
            .ok_or_else(|| ErrorKinds::NotExist("Schema not present".to_string()))?;
        let named_doc = schema.to_named_doc(&retrieved_doc);
        let mut s: String = "noexplain".to_string();
        if explain {
            s = query.explain(&searcher, doc_address)?.to_pretty_json();
        }
        debug!("retrieved doc {:?}", retrieved_doc.field_values());

        let mut hm: HashMap<u64, String> = HashMap::new();

        fields.iter().for_each(|&v| {
            if v > 0 {
                let e = match self.make_snippet(&v, &searcher, query, &retrieved_doc) {
                    Ok(g) => (v, g),
                    Err(e) => (-1, e.to_string()),
                };
                if e.0 >= 0 {
                    hm.insert(e.0 as u64, e.1);
                }
            }
        });

        let re = ResultElement {
            doc: named_doc,
            score: score as f32,
            explain: s,
            snippet_html: Some(hm),
        };
        self.return_buffer = serde_json::to_string(&re)?;
        Ok(0)
    }

    fn do_search(&mut self, params: serde_json::Value) -> InternalCallResult<u32> {
        const DEF_LIMIT: u64 = 10;
        let (top_limit, offset, explain, score, fields) = match params.as_object() {
            Some(p) => (
                p.get("top_limit")
                    .and_then(|u| u.as_u64())
                    .unwrap_or(DEF_LIMIT),
                p.get("offset").and_then(|u| u.as_u64()).unwrap_or(0) as usize,
                p.get("explain").and_then(|u| u.as_bool()).unwrap_or(false),
                p.get("scoring").and_then(|u| u.as_bool()).unwrap_or(true),
                p.get("snippet_field")
                    .and_then(|u| u.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.as_i64())
                            .collect::<Vec<i64>>()
                    })
                    .unwrap_or_else(|| vec![]), // Default to an empty vector
            ),
            None => (DEF_LIMIT, 0, false, true, vec![]),
        };
        let (query, idx, searcher) = self.setup_searcher()?;

        let td = self.do_search_execute(&searcher, query, idx, offset, top_limit, score)?;

        let snippets = !fields.is_empty();

        let mut hm = HashMap::new();

        debug!("search complete len = {}, td = {:?}", td.len(), td);
        let mut vret: Vec<ResultElement> = Vec::<ResultElement>::new();
        for (score, doc_address) in td {
            let retrieved_doc = searcher.doc(doc_address)?;
            let schema = self
                .schema
                .as_ref()
                .ok_or_else(|| ErrorKinds::NotExist("Schema not present".to_string()))?;
            let named_doc = schema.to_named_doc(&retrieved_doc);
            let mut s: String = "noexplain".to_string();
            if explain {
                s = query.explain(&searcher, doc_address)?.to_pretty_json();
            }
            if snippets {
                fields.iter().for_each(|v: &i64| {
                    if *v > 0 {
                        let e = match self.make_snippet(v, &searcher, query, &retrieved_doc) {
                            Ok(g) => (v, g),
                            Err(e) => (&(-1 as i64), e.to_string()),
                        };
                        if *e.0 >= 0 {
                            hm.insert(*e.0 as u64, e.1);
                        }
                    }
                });
            }
            debug!("retrieved doc {:?}", retrieved_doc.field_values());
            vret.append(&mut vec![ResultElement {
                doc: named_doc,
                score,
                explain: s,
                snippet_html: Some(hm.clone()),
            }]);
        }
        self.return_buffer = serde_json::to_string(&vret)?;
        debug!("ret = {}", self.return_buffer);
        Ok(0)
    }

    fn do_raw_search(&mut self, params: serde_json::Value) -> InternalCallResult<u32> {
        const DEF_LIMIT: u64 = 0;
        let mut limit = match params.as_object() {
            Some(p) => p.get("limit").and_then(|u| u.as_u64()).unwrap_or(DEF_LIMIT),
            None => DEF_LIMIT,
        };

        let (query, idx, searcher) = self.setup_searcher()?;

        if limit == 0 {
            limit = searcher.num_docs();
        }
        let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(
            &searcher,
        ))?;
        let schema = &idx.schema();
        let mut counter = 1u64;
        let mut vret: String = "[".to_string();
        let segr = searcher.segment_readers();
        for segment_reader in segr {
            let mut scorer = weight.scorer(segment_reader, 10.0)?;
            let store_reader = segment_reader.get_store_reader(10)?;
            loop {
                let doc_id = scorer.doc();
                if doc_id == TERMINATED {
                    break;
                }
                let doc = store_reader.get(doc_id)?;
                let named_doc = schema.to_named_doc(&doc);
                let match_string: String;
                vret.push_str(match serde_json::to_string(&named_doc) {
                    Ok(s) => {
                        if counter == 1 {
                            match_string = s;
                        } else {
                            match_string = format!(",{s}");
                        }
                        &match_string
                    }
                    Err(e) => {
                        return make_internal_json_error(ErrorKinds::Search(format!(
                            "json error = {e}"
                        )))
                    }
                });
                counter += 1;

                if counter > limit {
                    break;
                }
                scorer.advance();
            }
            if counter > limit {
                break;
            }
        }
        self.return_buffer = vret + "]";
        debug!("ret = {}", self.return_buffer);
        Ok(0)
    }

    pub fn handle_searcher(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> InternalCallResult<u32> {
        debug!("Searcher");
        let s = format!("{}", params);
        println!("{}", s);
        match method {
            "search" => self.do_search(params),
            "search_raw" => self.do_raw_search(params),
            "docset" => self.do_docset(params),
            "get_document" => self.do_get_document(params),
            _ => {
                error!("unknown method {method}");
                Err(ErrorKinds::NotExist(format!("unknown method {method}")))
            }
        }
    }
}
