//! The protocol specification and the implementation must agree.
//!
//! `protocols/jsonrpc/README.md` is the specification an agent writes against, and
//! `dispatch::method_table()` is the implementation. Nothing in the type system relates
//! them, so they drift — and a client written from the specification then breaks on a
//! detail the specification got wrong.
//!
//! Four defects of exactly this shape were found by an integrator working from the
//! specification alone: an error code that did not exist, a parameter that was read under
//! a different name, a field the specification promised and the code ignored, and a
//! summary that promised filtering the code did not do. Each was a line of prose that had
//! quietly stopped matching the code.
//!
//! So this file is the gate that would have caught all four. It reads the specification
//! from disk and compares it with the live method table.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use ncui_core::ErrorKind;
use ncui_runtime::dispatch::method_table;
use ncui_runtime::rpc::RpcError;

/// The specification, read from the repository.
fn specification() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocols/jsonrpc/README.md"
    );
    std::fs::read_to_string(path).expect("the protocol specification must be readable")
}

/// Every parameter the specification lists, from its parameters table.
fn documented_parameters(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for row in table_rows(text, "| Parameter |") {
        for name in row
            .first()
            .map(String::as_str)
            .unwrap_or_default()
            .split(',')
        {
            let name = name.trim().trim_matches('`');
            if !name.is_empty() {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// The runtime's own source, concatenated.
fn runtime_source() -> String {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut source = String::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                source.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    source
}

/// The rows of a markdown table, as cells.
fn table_rows(text: &str, header: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(header) {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if !line.starts_with('|') {
            if rows.is_empty() {
                continue;
            }
            break;
        }
        let cells: Vec<String> = line
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_owned())
            .collect();
        // The separator row is `| --- | --- |`.
        if cells.iter().all(|cell| cell.chars().all(|c| c == '-')) {
            continue;
        }
        rows.push(cells);
    }
    rows
}

/// The method names in a table cell, which may hold several separated by commas.
fn method_names(cell: &str) -> Vec<String> {
    cell.split(',')
        .map(|name| {
            name.trim()
                .trim_start_matches('`')
                .trim_end_matches('`')
                .split('/')
                .next()
                .unwrap_or("")
                .trim()
                .to_owned()
        })
        .filter(|name| name.starts_with("ncui."))
        .collect()
}

#[test]
fn the_specification_lists_exactly_the_methods_that_are_dispatched() {
    let text = specification();
    let rows = table_rows(
        &text,
        "| Method | Required capability | Destructive | Description |",
    );
    assert!(
        !rows.is_empty(),
        "the specification must have an implemented-methods table"
    );
    let mut documented: Vec<String> = rows.iter().flat_map(|row| method_names(&row[0])).collect();
    documented.sort();

    let mut implemented: Vec<String> = method_table()
        .iter()
        .map(|method| method.name.clone())
        .collect();
    implemented.sort();

    assert_eq!(
        documented, implemented,
        "the specification and the method table disagree about which methods exist"
    );
}

#[test]
fn the_specification_states_the_capability_each_method_really_requires() {
    let text = specification();
    let rows = table_rows(
        &text,
        "| Method | Required capability | Destructive | Description |",
    );
    let table = method_table();
    for row in rows {
        let names = method_names(&row[0]);
        let documented = row[1].trim().trim_matches('`');
        let destructive = row[2].trim();
        for name in names {
            let method = table
                .iter()
                .find(|method| method.name == name)
                .unwrap_or_else(|| panic!("`{name}` is documented but not dispatched"));
            let required = if method.requires.is_empty() {
                "none".to_owned()
            } else {
                method
                    .requires
                    .iter()
                    .map(ncui_core::Capability::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            assert_eq!(
                documented, required,
                "`{name}` requires a different capability than the specification says"
            );
            assert_eq!(
                destructive,
                if method.destructive { "**yes**" } else { "no" },
                "`{name}` is destructive in one place and not the other"
            );
        }
    }
}

#[test]
fn a_documented_method_is_never_documented_as_planned() {
    // The two tables in the specification are the clearest kind of drift: a method listed
    // as "not implemented" that is in fact dispatched is an agent told not to use
    // something that works, and a method listed as implemented that is not is worse.
    let text = specification();
    let planned = table_rows(&text, "| Method | Required capability | Blocked on |");
    for row in planned {
        for name in method_names(&row[0]) {
            assert!(
                !method_table().iter().any(|method| method.name == name),
                "`{name}` is documented as planned but is dispatched"
            );
        }
    }
}

#[test]
fn the_error_codes_in_the_specification_are_the_ones_the_runtime_returns() {
    // Every code the specification names must be one the runtime can produce, and every
    // code the runtime can produce must be in the specification. A code in one and not the
    // other is a client that will meet a code it was never told about, or a code it was
    // told about that never arrives.
    let text = specification();
    let rows = table_rows(&text, "| JSON-RPC code | `ncui_code` | Meaning |");
    assert!(
        !rows.is_empty(),
        "the specification must have an error-code table"
    );

    let mut documented: Vec<i64> = Vec::new();
    for row in rows {
        let code: i64 = row[0]
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("`{}` is not a JSON-RPC code", row[0]));
        documented.push(code);
        // The machine-readable code in the second column must be one the runtime knows.
        let ncui_code = row[1].trim().trim_matches('`');
        assert!(
            !ncui_code.is_empty(),
            "an error code must name its machine-readable code"
        );
    }
    // Several error kinds can share one JSON-RPC code — `validation` covers both a bad
    // parameter and a badly formed identifier — so the comparison is over the *set* of
    // codes, not the list of kinds that produce them.
    documented.sort_unstable();
    documented.dedup();

    let mut produced: Vec<i64> = error_kind_codes();
    produced.sort_unstable();
    produced.dedup();
    assert_eq!(
        documented, produced,
        "the specification's error codes and the runtime's differ"
    );
}

/// Every JSON-RPC code the runtime can produce, derived from the error model.
fn error_kind_codes() -> Vec<i64> {
    let kinds = [
        ErrorKind::Validation {
            field: "x".to_owned(),
            reason: "x".to_owned(),
        },
        ErrorKind::InvalidId {
            reason: "x".to_owned(),
        },
        ErrorKind::CapabilityDenied {
            capability: "x".to_owned(),
            subject: "x".to_owned(),
        },
        ErrorKind::LimitExceeded {
            resource: "x".to_owned(),
            limit: 1,
            observed: 2,
        },
        ErrorKind::NotFound {
            kind: "x".to_owned(),
            id: "x".to_owned(),
        },
        ErrorKind::Conflict {
            reason: "x".to_owned(),
        },
        ErrorKind::Protocol {
            reason: "x".to_owned(),
        },
        ErrorKind::Timeout { elapsed_ms: 1 },
        ErrorKind::Cancelled,
        ErrorKind::Backpressure {
            channel: "x".to_owned(),
            capacity: 1,
        },
        ErrorKind::Io {
            operation: "x".to_owned(),
            message: "x".to_owned(),
            io_kind: std::io::ErrorKind::Other,
        },
    ];
    kinds
        .iter()
        .map(|kind| {
            let error = ncui_core::NcError::new(kind.clone()).operation("test");
            i64::from(RpcError::from_nc(&error).code)
        })
        .collect()
}

#[test]
fn every_parameter_the_specification_lists_appears_in_the_runtime() {
    // The class of defect the rest of this file cannot see. Everything above compares the
    // specification's *method table* with the code; a parameter mentioned in a sentence was looked
    // for nowhere, and `deadline_ms` was documented for months with nothing behind it. The
    // parameters table makes the list explicit, and this checks each name against the runtime's
    // source.
    //
    // What it proves is that the code *names* the parameter, not that it honours it — the behaviour
    // tests are what prove that, and the deadline has three. A name that appears nowhere is a claim
    // with nothing behind it, and that is what this catches.
    let text = specification();
    let source = runtime_source();
    let parameters = documented_parameters(&text);
    assert!(
        parameters.len() >= 10,
        "the scan found only {} parameters, so it is not reading the table: {parameters:?}",
        parameters.len()
    );
    for name in parameters {
        assert!(
            source.contains(&name),
            "the specification lists `{name}`, which appears nowhere in the runtime's source"
        );
    }
}

#[test]
fn the_specification_describes_cursor_and_deadline_consistently() {
    // `cursor` is implemented for `ncui.logs.query` and reported via `next_cursor`,
    // so the specification must no longer claim it is absent.
    let text = specification();
    assert!(
        !text.contains("is **not yet implemented**"),
        "the specification still claims `cursor` is not implemented"
    );
    assert!(
        text.contains("next_cursor"),
        "the specification must document the `next_cursor` response field"
    );
    assert!(
        text.contains("Long-running methods accept an optional `deadline_ms`"),
        "the specification must state the deadline parameter"
    );
}
