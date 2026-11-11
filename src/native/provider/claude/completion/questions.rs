use super::records::{Binding, MAX_INDEX, QuestionRecord, position};
use crate::native::provider::{
    EmittedQuestion, QuestionEmission, QuestionEmissions, QuestionObservation,
};
use crate::native::session::{Reader, Store, requests};
use anyhow::{Result, bail};
use serde_json::Value;

// The query's existing lifecycle lock protects this historical Receipt; never consult
// the current pending turn or infer that these questions still require an answer.
pub(in crate::native::provider) fn read(
    reader: &Reader,
    receipt: &requests::Receipt,
) -> Option<QuestionEmissions> {
    match QuestionRecord::read(reader, receipt) {
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
        deflected: false,
    })
}

// The saved record of the Claim's Receipt, or an empty one, with its encoding before any change.
fn saved(store: &Store, binding: &Binding) -> Result<Option<(QuestionRecord, Vec<u8>)>> {
    let Some(receipt) = requests::for_claim(store, &binding.claim)? else {
        return Ok(None);
    };
    let saved = QuestionRecord::read(store, &receipt)?
        .unwrap_or_else(|| QuestionRecord::empty(receipt.request_id, binding));
    if saved.binding != *binding {
        bail!("Claude question binding cannot be replaced");
    }
    let before = serde_json::to_vec(&saved)?;
    Ok(Some((saved, before)))
}

// Completion validates the immutable original turn binding and live Claim under this
// same lock. An unbound continuation supplies no question attribution authority.
pub(super) fn publish(store: &Store, binding: &Binding, data: &Value) -> Result<()> {
    let Some((mut saved, before)) = self::saved(store, binding)? else {
        return Ok(());
    };
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
                // A replay compares content only: a recorded deflection is not a conflict.
                saved.incomplete |= previous.questions != item.questions;
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
            if !saved.fits() {
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
        saved.write(store)?;
    }
    Ok(())
}

// The question plugin refused the call at `step`/`index` before any dialog. Under the same
// lock and binding rules as `publish`, this sets the deflected flag of the retained item at
// that position: the one change a saved item may receive, still within the record limit. The
// position alone does not name the call: a repeated step keeps the first question retained
// there, so the flag is set only when the refused questions project to the retained item's
// text and option labels. An item the record lacks (its emission report was lost, or the
// record is full), a different question at the position and malformed data are never turned
// into a flag: the observation becomes incomplete. Repeats are idempotent.
pub(super) fn deflect(store: &Store, binding: &Binding, data: &Value) -> Result<()> {
    let Some((mut saved, before)) = self::saved(store, binding)? else {
        return Ok(());
    };
    let number = |key| {
        data.get(key)
            .and_then(Value::as_u64)
            .filter(|value| *value <= MAX_INDEX)
    };
    let refused = number("step")
        .zip(number("index"))
        .and_then(|(step, index)| {
            let questions = data.get("questions")?;
            emission(
                step,
                &serde_json::json!({"index": index, "questions": questions}),
            )
        });
    match refused.and_then(|refused| {
        saved
            .items
            .iter()
            .position(|item| item.id == refused.id && item.questions == refused.questions)
    }) {
        Some(found) if !saved.items[found].deflected => {
            saved.items[found].deflected = true;
            if !saved.fits() {
                saved.items[found].deflected = false;
                saved.incomplete = true;
                saved.full = true;
            }
        }
        Some(_) => {}
        None => saved.incomplete = true,
    }
    if serde_json::to_vec(&saved)? != before {
        saved.write(store)?;
    }
    Ok(())
}
