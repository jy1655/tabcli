use super::{Binding, valid_claude_conversation_id, valid_identity};
use crate::native::provider::{
    EmittedQuestion, QuestionEmission, QuestionEmissions, QuestionObservation,
};
use crate::native::session::{Reader, Store, requests};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const RECORD_BYTES: u64 = 64 * 1024;
const MAX_INDEX: u64 = 9_007_199_254_740_991;

#[derive(Deserialize, Serialize)]
struct Record {
    schema: u32,
    request_id: String,
    binding: Binding,
    items: Vec<QuestionEmission>,
    incomplete: bool,
    full: bool,
}

fn name(token: &str) -> String {
    format!("claude-questions-{token}.json")
}

fn position(id: &str) -> Option<(u64, u64)> {
    let (step, tool) = id.strip_prefix("step:")?.split_once("/tool:")?;
    let step: u64 = step.parse().ok()?;
    let tool: u64 = tool.parse().ok()?;
    (step <= MAX_INDEX && tool <= MAX_INDEX && id == format!("step:{step}/tool:{tool}"))
        .then_some((step, tool))
}

fn fits(record: &Record) -> bool {
    struct Limit(u64);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len() as u64)
                .ok_or_else(|| std::io::Error::other("Claude question record limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer_pretty(Limit(RECORD_BYTES), record).is_ok()
}

fn record(reader: &Reader, receipt: &requests::Receipt) -> Result<Option<Record>> {
    let Some(bytes) = reader
        .private(&name(&receipt.claim_token))
        .timeline(RECORD_BYTES)?
    else {
        return Ok(None);
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    if record.schema != 1
        || record.request_id != receipt.request_id
        || record.binding.schema != 1
        || record.binding.claim != receipt.claim_token
        || !valid_claude_conversation_id(&record.binding.conversation)
        || !valid_identity(&record.binding.turn_id)
        || !valid_identity(&record.binding.correlation_id)
        || (record.full && !record.incomplete)
        || !fits(&record)
    {
        bail!("invalid Claude question record");
    }
    let mut previous = None;
    for item in &record.items {
        let current = position(&item.id).context("invalid Claude question position")?;
        if previous.is_some_and(|previous| previous >= current)
            || !(1..=4).contains(&item.questions.len())
            || item.questions.iter().any(|q| {
                q.title.trim().is_empty()
                    || q.options.as_ref().is_none_or(|options| {
                        !(2..=4).contains(&options.len())
                            || options.iter().any(|label| label.trim().is_empty())
                    })
            })
        {
            bail!("invalid Claude retained question prefix");
        }
        previous = Some(current);
    }
    Ok(Some(record))
}

// The query's existing lifecycle lock protects this historical Receipt; never consult
// the current pending turn or infer that these questions still require an answer.
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
            items: vec![],
            observation: QuestionObservation::Unreadable,
        }),
    }
}

fn emission(step: u64, item: &Value) -> Option<QuestionEmission> {
    let index = item
        .get("index")?
        .as_u64()
        .filter(|index| *index <= MAX_INDEX)?;
    let questions = item.get("questions")?.as_array()?;
    if !(1..=4).contains(&questions.len()) {
        return None;
    }
    let mut emitted = Vec::with_capacity(questions.len());
    for question in questions {
        let title = question
            .get("question")?
            .as_str()
            .filter(|s| !s.trim().is_empty())?;
        question.get("header")?.as_str()?;
        question.get("multiSelect")?.as_bool()?;
        let options = question.get("options")?.as_array()?;
        if !(2..=4).contains(&options.len()) {
            return None;
        }
        let labels = options
            .iter()
            .map(|option| {
                option.get("description")?.as_str()?;
                Some(
                    option
                        .get("label")?
                        .as_str()
                        .filter(|s| !s.trim().is_empty())?
                        .to_owned(),
                )
            })
            .collect::<Option<Vec<_>>>()?;
        emitted.push(EmittedQuestion {
            title: title.to_owned(),
            options: Some(labels),
        });
    }
    Some(QuestionEmission {
        id: format!("step:{step}/tool:{index}"),
        questions: emitted,
    })
}

// Completion validates the immutable original turn binding and live Claim under this
// same lock. An unbound continuation supplies no question attribution authority.
pub(super) fn publish(store: &Store, binding: &Binding, data: &Value) -> Result<()> {
    let Some(receipt) = requests::for_claim(store, &binding.claim)? else {
        return Ok(());
    };
    let mut saved = record(store, &receipt)?.unwrap_or_else(|| Record {
        schema: 1,
        request_id: receipt.request_id,
        binding: binding.clone(),
        items: vec![],
        incomplete: false,
        full: false,
    });
    if saved.binding != *binding {
        bail!("Claude question binding cannot be replaced");
    }
    let before = serde_json::to_vec(&saved)?;
    let items = data.get("items").and_then(Value::as_array);
    let step = data
        .get("step")
        .and_then(Value::as_u64)
        .filter(|step| *step <= MAX_INDEX);
    let incomplete = data.get("incomplete").and_then(Value::as_bool);
    if let (Some(items), Some(step), Some(incomplete)) = (items, step, incomplete) {
        saved.incomplete |= incomplete;
        for item in items {
            let Some(item) = emission(step, item) else {
                saved.incomplete = true;
                continue;
            };
            if let Some(previous) = saved.items.iter().find(|previous| previous.id == item.id) {
                saved.incomplete |= previous != &item;
                continue;
            }
            if saved.full {
                continue;
            }
            if saved
                .items
                .last()
                .is_some_and(|previous| position(&previous.id) >= position(&item.id))
            {
                saved.incomplete = true;
                continue;
            }
            saved.items.push(item);
            if !fits(&saved) {
                saved.items.pop();
                saved.incomplete = true;
                saved.full = true;
            }
        }
    } else {
        saved.incomplete = true;
    }
    // Empty healthy observations add no evidence. Retries never rewrite a saved prefix.
    if (!saved.items.is_empty() || saved.incomplete) && serde_json::to_vec(&saved)? != before {
        store.private(&name(&binding.claim)).write_json(&saved)?;
    }
    Ok(())
}
