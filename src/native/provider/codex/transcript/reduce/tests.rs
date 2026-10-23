use super::*;
use serde_json::Value;

/// The watcher's overflow path, fed from the prefix up to `split` and then byte by byte.
fn reduced(record: &[u8], split: usize, elide: usize) -> Observation<Vec<u8>> {
    let mut reducer = Reducer::reduce(record[..split].to_vec(), elide)?;
    for byte in &record[split..] {
        reducer.push(*byte)?;
    }
    let reduced = reducer.finish()?;
    let text = std::str::from_utf8(&reduced).map_err(|_| "invalid_utf8")?;
    serde_json::from_str::<Value>(text).map_err(|_| "invalid_json")?;
    Ok(reduced)
}
/// What the ordinary path reports for the same record.
fn ordinary(record: &[u8]) -> Observation<()> {
    let text = std::str::from_utf8(record).map_err(|_| "invalid_utf8")?;
    serde_json::from_str::<Value>(text).map_err(|_| "invalid_json")?;
    Ok(())
}
/// Every split agrees, and validity is serde's. A fault can be found earlier in byte
/// order than the ordinary path's whole-line UTF-8 check, so only a line that is valid
/// UTF-8 must report `invalid_json`.
fn assert_agrees(record: &[u8], elide: usize) -> Observation<Vec<u8>> {
    let whole = reduced(record, record.len(), elide);
    for split in 0..record.len() {
        assert_eq!(
            reduced(record, split, elide),
            whole,
            "{record:?} at {split}"
        );
    }
    match (&whole, ordinary(record)) {
        (Ok(_), Ok(())) => {}
        (Err("invalid_utf8"), Err("invalid_utf8")) => {}
        (Err("invalid_json"), Err(_)) if std::str::from_utf8(record).is_err() => {}
        (Err("invalid_json"), Err("invalid_json")) => {}
        (whole, ordinary) => panic!("{record:?}: {whole:?} != {ordinary:?}"),
    }
    whole
}

#[test]
fn bounded_string_corpus_matches_serde_at_every_split_and_cut_off() {
    let tokens: [&[u8]; 20] = [
        b"\\",
        b"u",
        b"\"",
        b"a",
        b"\x01",
        b"\t",
        b"\x7f",
        b"\xc3",
        b"\xa9",
        b"\xff",
        "é".as_bytes(),
        b"\\n",
        b"\\uD83D",
        b"\\uDE00",
        b"\\uDBFF",
        b"\\u0041",
        b"\\uD8",
        b"00",
        b"\\u",
        b"DC",
    ];
    let mut bodies = vec![Vec::new()];
    let mut layer = vec![Vec::new()];
    for _ in 0..3 {
        layer = layer
            .iter()
            .flat_map(|body: &Vec<u8>| tokens.iter().map(move |token| [body, *token].concat()))
            .collect();
        bodies.extend(layer.iter().cloned());
    }
    let mut accepted = 0;
    for body in bodies {
        let record = [b"[\"".as_slice(), &body, b"\",0]"].concat();
        let keep = assert_agrees(&record, usize::MAX);
        if let Ok(kept) = &keep {
            accepted += 1;
            assert_eq!(kept, &record);
        }
        for elide in [0, 3] {
            let cut = reduced(&record, record.len(), elide);
            assert_eq!(cut.is_ok(), keep.is_ok(), "{record:?} cut at {elide}");
            if let Ok(cut) = cut
                && body.len() > elide
            {
                assert_eq!(cut, b"[\"\",0]");
            }
        }
    }
    assert!(accepted > 100);
}

#[test]
fn faults_past_the_cut_off_are_found_across_prefix_and_pushed_bytes() {
    let fill = "A".repeat(ELIDE_BYTES + 10);
    let cases: [(&[u8], Observation<()>); 30] = [
        (b"\xff", Err("invalid_utf8")),
        (b"\xc0\x80", Err("invalid_utf8")),
        (b"\xed\xa0\x80", Err("invalid_utf8")),
        (b"\xf4\x90\x80\x80", Err("invalid_utf8")),
        (b"\xe2\x82A", Err("invalid_utf8")),
        (b"\xe2\x82", Err("invalid_utf8")),
        (b"\\x", Err("invalid_json")),
        (b"\\u12G4", Err("invalid_json")),
        (b"\\uDC00", Err("invalid_json")),
        (b"\\uD800\"", Err("invalid_json")),
        (b"\\uD800\\n", Err("invalid_json")),
        (b"\\uD800\\u0041", Err("invalid_json")),
        (b"\\uD800\\uD800", Err("invalid_json")),
        (b"\\uD800", Err("invalid_json")),
        (b"\x01", Err("invalid_json")),
        (b"\t", Err("invalid_json")),
        (b"\n", Err("invalid_json")),
        (b"\x1f", Err("invalid_json")),
        (b"\\uD83D\\uDE00", Ok(())),
        (b"\\uDBFF\\uDFFF", Ok(())),
        (b"\\ud83d\\ude00", Ok(())),
        (b"\\\"\\\\\\/\\b\\f\\n\\r\\t", Ok(())),
        (b"\\u0000\\u001F\\uFFFF", Ok(())),
        ("😀".as_bytes(), Ok(())),
        ("한국어".as_bytes(), Ok(())),
        (b"\x7f", Ok(())),
        (b"\xf4\x8f\xbf\xbf", Ok(())),
        (b"\xef\xbf\xbf", Ok(())),
        (b"\xc2\x80", Ok(())),
        (b"'", Ok(())),
    ];
    for (fault, expected) in cases {
        let head = br#"{"type":"response_item","payload":{"type":"t","output":""#;
        // The unfinished sequence `\xe2\x82` also reaches the line end unfinished.
        let tail: &[u8] = if fault == b"\xe2\x82" {
            b""
        } else {
            br#"zz"}}"#
        };
        let record = [head.as_slice(), fill.as_bytes(), fault, tail].concat();
        let at = head.len() + fill.len();
        for split in [
            0,
            at,
            at + 1,
            at + fault.len().saturating_sub(1),
            record.len(),
        ] {
            let result = reduced(&record, split.min(record.len()), ELIDE_BYTES);
            match expected {
                Ok(()) => assert_eq!(
                    result.unwrap(),
                    br#"{"type":"response_item","payload":{"type":"t","output":""}}"#,
                    "{fault:?} at {split}"
                ),
                Err(reason) => assert_eq!(result, Err(reason), "{fault:?} at {split}"),
            }
        }
        assert_eq!(ordinary(&record), expected, "{fault:?}");
    }
}

#[test]
fn every_long_string_is_cut_and_grammar_is_still_parsed() {
    let image = "A".repeat(ELIDE_BYTES + 1);
    let kept = "B".repeat(ELIDE_BYTES);
    let record = format!(
        r#"{{"type":"response_item","payload":{{"type":"custom_tool_call_output","output":[{{"type":"input_image","image_url":"{image}"}},{{"type":"input_image","image_url":"{image}","{image}":"{kept}"}}]}},"metadata":{{"k":"{image}"}}}}"#
    );
    assert_eq!(
        String::from_utf8(reduced(record.as_bytes(), 7, ELIDE_BYTES).unwrap()).unwrap(),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"custom_tool_call_output","output":[{{"type":"input_image","image_url":""}},{{"type":"input_image","image_url":"","":"{kept}"}}]}},"metadata":{{"k":""}}}}"#
        )
    );
    // Escapes count as their written bytes, so `\u0041` is six of the cut-off.
    let escaped = format!(r#"["{}\u0041"]"#, "A".repeat(ELIDE_BYTES - 6));
    assert_eq!(
        reduced(escaped.as_bytes(), 0, ELIDE_BYTES).unwrap(),
        escaped.as_bytes()
    );
    let escaped = format!(r#"["{}\u0041"]"#, "A".repeat(ELIDE_BYTES - 5));
    assert_eq!(
        reduced(escaped.as_bytes(), 0, ELIDE_BYTES).unwrap(),
        br#"[""]"#
    );
    for damaged in [
        format!(r#"{{"a":"{image}"x}}"#),
        format!(r#"{{"a":"{image}",}}"#),
        format!(r#"{{"a":"{image}"}} {{}}"#),
        format!(r#"{{"a":"{image}","n":1e400}}"#),
        format!(r#"{{"a":"{image}","n":-1e400}}"#),
        format!(r#"{{"a":"{image}","n":NaN}}"#),
        format!(r#"{{"a":"{image}","n":01}}"#),
        format!(
            r#"{{"a":"{image}","b":{}{}}}"#,
            "[".repeat(128),
            "]".repeat(128)
        ),
        format!(r#"{{"a":"{image}" "b":1}}"#),
        format!(r#"["{image}"#),
    ] {
        assert_eq!(
            reduced(damaged.as_bytes(), 3, ELIDE_BYTES),
            Err("invalid_json"),
            "{}",
            &damaged[damaged.len() - 20..]
        );
        assert_eq!(ordinary(damaged.as_bytes()), Err("invalid_json"));
    }
    let deep = format!(
        r#"{{"a":"{image}","b":{}{}}}"#,
        "[".repeat(126),
        "]".repeat(126)
    );
    reduced(deep.as_bytes(), 3, ELIDE_BYTES).unwrap();
    ordinary(deep.as_bytes()).unwrap();
}

#[test]
fn reduced_record_over_line_bytes_is_oversized_and_retention_is_bounded() {
    // `[0,0,...,0]` of an odd length: nothing to cut.
    let numbers = |len: usize| {
        assert_eq!(len % 2, 1);
        let mut record = vec![b'0'; len];
        record[0] = b'[';
        for byte in record.iter_mut().skip(2).step_by(2) {
            *byte = b',';
        }
        record[len - 1] = b']';
        record
    };
    let over = numbers(LINE_BYTES + 1);
    let mut reducer = Reducer::from_prefix(over[..LINE_BYTES].to_vec()).unwrap();
    assert_eq!(reducer.push(b']'), Err("oversized"));
    // After a cut string the record ends at exactly `LINE_BYTES`, not one byte later.
    let shrunk = &over[..LINE_BYTES - 8];
    let cut = b",\"".iter().chain(&[b'c'; ELIDE_BYTES + 1]).chain(b"\"");
    let mut reducer = Reducer::from_prefix(shrunk.to_vec()).unwrap();
    for byte in cut.clone().chain(b"]    ") {
        reducer.push(*byte).unwrap();
    }
    assert_eq!(reducer.push(b' '), Err("oversized"));
    // A kept string ending past it is oversized at its closing quote, and the buffer
    // grows from the shrunk prefix only as far as the bound.
    let mut reducer = Reducer::from_prefix(shrunk.to_vec()).unwrap();
    assert_eq!(reducer.retained(), LINE_BYTES - 8);
    for byte in cut.chain(b",\"").chain(&[b'k'; ELIDE_BYTES]) {
        reducer.push(*byte).unwrap();
    }
    assert!(reducer.retained() <= LINE_BYTES + ELIDE_BYTES);
    assert_eq!(reducer.push(b'"'), Err("oversized"));
}

#[test]
fn repeated_discriminators_are_counted_after_unescaping() {
    for (text, repeated) in [
        (r#"{"type":"a","payload":{"type":"b"}}"#, false),
        (
            r#"{"payload":{"type":"b","x":{"type":1}},"type":"a","m":{"payload":1}}"#,
            false,
        ),
        (r#"{"type":"a","type":"a","payload":{"type":"b"}}"#, true),
        (
            r#"{"payload":{"type":"b"},"type":"response_item","type":"x"}"#,
            true,
        ),
        (
            r#"{"type":"a","payload":{"type":"b"},"payload":{"type":"b"}}"#,
            true,
        ),
        (
            r#"{"type":"a","payload":{"type":"b","type":"message"}}"#,
            true,
        ),
        (
            r#"{"type":"a","payload":{"type":"message","type":"b"}}"#,
            true,
        ),
        (
            r#"{"\u0074ype":"a","type":"a","payload":{"type":"b"}}"#,
            true,
        ),
        (
            r#"{"type":"a","payload":{"typ\u0065":"b","type":"b"}}"#,
            true,
        ),
        (r#"{"type":"a","payload":[{"type":"b"}]}"#, true),
        (r#"[{"type":"a"}]"#, true),
    ] {
        assert_eq!(repeated_discriminator(text), repeated, "{text}");
    }
}
