use super::lifecycle::{Binding, current_claim, read_binding};
use crate::native::provider::{
    EmittedQuestion, QuestionEmission, QuestionEmissions, QuestionObservation,
};
use crate::native::session::{Reader, RecordStore, Store, requests};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize, ser::SerializeSeq};
use serde_json::Value;
use std::{io::Write, path::Path};

const QUESTION_RECORD_BYTES: u64 = 64 * 1024;

#[derive(Deserialize, Serialize)]
struct Record {
    schema: u32,
    request_id: String,
    claim_token: String,
    items: Vec<QuestionEmission>,
    incomplete: bool,
}

#[derive(Serialize)]
struct RecordRef<'a> {
    schema: u32,
    request_id: &'a str,
    claim_token: &'a str,
    items: &'a [QuestionEmission],
    incomplete: bool,
}

fn name(token: &str) -> String {
    format!("codex-questions-{token}.json")
}

fn valid_item(item: &QuestionEmission) -> bool {
    !item.id.is_empty()
        && !item.questions.is_empty()
        && item.questions.iter().all(|q| {
            !q.title.trim().is_empty()
                && q.options.as_ref().is_none_or(|options| {
                    !options.is_empty() && options.iter().all(|s| !s.trim().is_empty())
                })
        })
}

fn record(reader: &Reader, receipt: &requests::Receipt) -> Result<Option<Record>> {
    let Some(bytes) = reader
        .private(&name(&receipt.claim_token))
        .timeline(QUESTION_RECORD_BYTES)?
    else {
        return Ok(None);
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    if record.schema != 1
        || record.request_id != receipt.request_id
        || record.claim_token != receipt.claim_token
        || !record.items.iter().all(valid_item)
        || !fits(&record)
        || record
            .items
            .iter()
            .enumerate()
            .any(|(i, item)| record.items[..i].iter().any(|p| p.id == item.id))
    {
        bail!("invalid Codex question record");
    }
    Ok(Some(record))
}

pub(in crate::native::provider) fn read(
    reader: &Reader,
    receipt: &requests::Receipt,
) -> Option<QuestionEmissions> {
    match record(reader, receipt) {
        Ok(None) => None,
        Ok(Some(record)) => Some(QuestionEmissions {
            items: record.items,
            observation: if record.incomplete {
                QuestionObservation::Incomplete
            } else {
                QuestionObservation::Complete
            },
        }),
        Err(_) => Some(QuestionEmissions {
            items: Vec::new(),
            observation: QuestionObservation::Unreadable,
        }),
    }
}

struct SizeLimit(u64);
impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("question record limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn fits(value: &impl Serialize) -> bool {
    serde_json::to_writer_pretty(SizeLimit(QUESTION_RECORD_BYTES), value).is_ok()
}

#[derive(Serialize)]
struct ItemRef<'a> {
    id: &'a str,
    questions: QuestionsRef<'a>,
}
struct QuestionsRef<'a>(&'a [Value]);
impl Serialize for QuestionsRef<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct QuestionRef<'a> {
            title: &'a str,
            options: Option<&'a Vec<Value>>,
        }
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for question in self.0 {
            sequence.serialize_element(&QuestionRef {
                title: question["title"].as_str().unwrap(),
                options: question["options"].as_array(),
            })?;
        }
        sequence.end()
    }
}

#[derive(Default)]
pub(super) struct Questions {
    items: Vec<QuestionEmission>,
    incomplete: bool,
    stored: usize,
    stored_incomplete: bool,
    full: bool,
    retired: bool,
}

impl Questions {
    pub(super) fn dirty(&self) -> bool {
        !self.retired
            && (self.items.len() > self.stored || self.incomplete != self.stored_incomplete)
    }

    pub(super) fn notice(&mut self, payload: &Value, binding: &Binding, accepted: bool) {
        let item = &payload["item"];
        if self.full
            || self.retired
            || payload["type"] != "item_completed"
            || item["type"] != "AgentMessage"
            || item["delivery"] != "async"
            || item["questions"].is_null()
        {
            return;
        }
        if !accepted
            || payload
                .get("thread_id")
                .is_some_and(|id| id.as_str() != Some(&binding.thread_id))
        {
            self.incomplete = true;
            return;
        }
        let Some(id) = item["id"].as_str().filter(|s| !s.is_empty()) else {
            self.incomplete = true;
            return;
        };
        let Some(questions) = item["questions"].as_array().filter(|q| !q.is_empty()) else {
            self.incomplete = true;
            return;
        };
        for question in questions {
            if question["title"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty())
                || (!question["options"].is_null()
                    && question["options"].as_array().is_none_or(|options| {
                        options.is_empty()
                            || options
                                .iter()
                                .any(|s| s.as_str().is_none_or(|s| s.trim().is_empty()))
                    }))
            {
                self.incomplete = true;
                return;
            }
        }
        // Measure borrowed content before allocating strings from an up-to-8-MiB source line.
        if !fits(&ItemRef {
            id,
            questions: QuestionsRef(questions),
        }) {
            self.incomplete = true;
            self.full = true;
            return;
        }
        let candidate = QuestionEmission {
            id: id.to_owned(),
            questions: questions
                .iter()
                .map(|q| EmittedQuestion {
                    title: q["title"].as_str().unwrap().to_owned(),
                    options: q["options"].as_array().map(|options| {
                        options
                            .iter()
                            .map(|s| s.as_str().unwrap().to_owned())
                            .collect()
                    }),
                })
                .collect(),
        };
        if let Some(first) = self.items.iter().find(|first| first.id == id) {
            self.incomplete |= first != &candidate;
            return;
        }
        self.items.push(candidate);
        // The Receipt supplies the real request id under the publication lock. The
        // empty id bounds memory here; flush charges the complete record before writing.
        if !fits(&self.view("", &binding.claim_token)) {
            self.items.pop();
            self.incomplete = true;
            self.full = true;
        }
    }

    fn view<'a>(&'a self, request_id: &'a str, claim_token: &'a str) -> RecordRef<'a> {
        RecordRef {
            schema: 1,
            request_id,
            claim_token,
            items: &self.items,
            incomplete: self.incomplete,
        }
    }

    pub(super) fn flush(&mut self, directory: &Path, binding: &Binding) {
        let _ = self.publish(directory, binding);
    }

    fn publish(&mut self, directory: &Path, binding: &Binding) -> Result<()> {
        let store = Store::open_unchecked(directory);
        let _lock = store.lock()?;
        if !current_claim(directory, &binding.claim_token)?
            || read_binding(directory, &binding.claim_token)?.as_ref() != Some(binding)
        {
            self.retired = true;
            return Ok(());
        }
        let Some(receipt) = requests::for_claim(&store, &binding.claim_token)? else {
            return Ok(());
        };
        let saved = record(&store, &receipt)?;
        if let Some(saved) = &saved {
            self.incomplete |= saved.incomplete;
        }
        while !fits(&self.view(&receipt.request_id, &receipt.claim_token)) {
            if self.items.pop().is_none() {
                self.retired = true;
                return Ok(());
            }
            self.incomplete = true;
            self.full = true;
        }
        if let Some(saved) = &saved {
            if saved.items.starts_with(&self.items) {
                if self.incomplete && !saved.incomplete {
                    let updated = RecordRef {
                        schema: 1,
                        request_id: &receipt.request_id,
                        claim_token: &receipt.claim_token,
                        items: &saved.items,
                        incomplete: true,
                    };
                    write(&store.private(&name(&receipt.claim_token)), &updated)?;
                }
                self.stored = saved.items.len();
                self.stored_incomplete = self.incomplete;
                return Ok(());
            }
            if !self.items.starts_with(&saved.items) {
                self.retired = true;
                return Ok(());
            }
        }
        write(
            &store.private(&name(&receipt.claim_token)),
            &self.view(&receipt.request_id, &receipt.claim_token),
        )?;
        self.stored = self.items.len();
        self.stored_incomplete = self.incomplete;
        Ok(())
    }
}

fn write(target: &RecordStore, record: &impl Serialize) -> Result<()> {
    #[cfg(test)]
    {
        tests::write(target, record)
    }
    #[cfg(not(test))]
    {
        target.write_json(record)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    thread_local! { static WRITE_FAULT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
    pub(in crate::native::provider::codex) fn with_write_fault<T>(
        budget: usize,
        run: impl FnOnce() -> T,
    ) -> T {
        WRITE_FAULT.with(|cell| cell.set(Some(budget)));
        let result = run();
        WRITE_FAULT.with(|cell| cell.set(None));
        result
    }
    pub(super) fn write(target: &RecordStore, record: &impl Serialize) -> Result<()> {
        match WRITE_FAULT.with(|cell| cell.get()) {
            Some(budget) => crate::native::session::primitives::with_fault_budget(budget, || {
                target.write_json(record)
            }),
            None => target.write_json(record),
        }
    }
}
