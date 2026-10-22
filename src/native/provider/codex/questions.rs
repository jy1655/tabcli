use super::turn::{
    Binding, QuestionRecordRef as RecordRef, binding as read_binding, current_claim,
    question_fits as fits, question_name as name, questions as record,
};
use crate::native::provider::{
    EmittedQuestion, QuestionEmission, QuestionEmissions, QuestionObservation,
};
use crate::native::session::{Reader, RecordStore, Store, requests};
use anyhow::Result;
use serde::{Serialize, ser::SerializeSeq};
use serde_json::Value;
use std::path::Path;

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
        if !current_claim(&store, &binding.claim_token)?
            || read_binding(&store, &binding.claim_token)?.as_ref() != Some(binding)
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
