//! Tests for the MCP `read_memory` tool: multi-id reads, full untruncated content, the
//! not-found/unauthorized indistinguishability contract, and the admin-gated system-role
//! reveal. Hermetic — no transport, no network. Episodes are seeded directly into the store
//! (the Capturer refuses system-role writes, so a direct insert is the only way to place a
//! `Role::System` turn), then read back through the tool. The all-lifecycle-kind reads live
//! in the sibling `read_memory_multikind.rs` binary; both share `read_memory_support`.

mod read_memory_support;
use read_memory_support::*;

#[test]
fn reads_every_requested_id_in_order_with_a_requested_found_header() {
    let memory = memory();
    let alice = Id::generate();
    let ns = Namespace::Agent(alice.to_string());
    let a = seed(&memory, "first memory body", ns.clone(), Role::Assistant);
    let b = seed(&memory, "second memory body", ns.clone(), Role::User);
    let c = seed(&memory, "third memory body", ns, Role::Assistant);

    let out = read_memory_tool(
        &memory,
        read_params(&[a, b, c], alice),
        None,
        AuthEnabled(false),
    )
    .expect("read");
    assert!(
        out.starts_with("[read_memory] requested=3 found=3"),
        "{out}"
    );
    // All three present, each in its own <memory> line, in request order.
    let first = out.find("first memory body").expect("first present");
    let second = out.find("second memory body").expect("second present");
    let third = out.find("third memory body").expect("third present");
    assert!(
        first < second && second < third,
        "request order preserved: {out}"
    );
    assert_eq!(
        out.matches("<memory ").count(),
        3,
        "one line per found id: {out}"
    );
}

#[test]
fn a_missing_id_is_simply_absent_not_an_error() {
    let memory = memory();
    let alice = Id::generate();
    let ns = Namespace::Agent(alice.to_string());
    let real_one = seed(&memory, "real memory one", ns.clone(), Role::Assistant);
    let never_stored = Id::generate();
    let real_two = seed(&memory, "real memory two", ns, Role::Assistant);

    let out = read_memory_tool(
        &memory,
        read_params(&[real_one, never_stored, real_two], alice),
        None,
        AuthEnabled(false),
    )
    .expect("a missing id is best-effort, not a call-level error");
    assert!(
        out.starts_with("[read_memory] requested=3 found=2"),
        "{out}"
    );
    assert!(out.contains("real memory one"), "{out}");
    assert!(out.contains("real memory two"), "{out}");
}

#[test]
fn an_unauthorized_id_is_indistinguishable_from_a_missing_one() {
    let memory = memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let alice_id = seed(
        &memory,
        "alice private body",
        Namespace::Agent(alice.to_string()),
        Role::Assistant,
    );
    let bob_id = seed(
        &memory,
        "bob private body",
        Namespace::Agent(bob.to_string()),
        Role::Assistant,
    );

    // Alice requests her own id plus Bob's. Bob's is in a namespace she cannot see, so it
    // drops out of the found set exactly like a missing id — the header reveals only a count.
    let out = read_memory_tool(
        &memory,
        read_params(&[alice_id, bob_id], alice),
        None,
        AuthEnabled(false),
    )
    .expect("read");
    assert!(
        out.starts_with("[read_memory] requested=2 found=1"),
        "{out}"
    );
    assert!(out.contains("alice private body"), "{out}");
    assert!(
        !out.contains("bob private body"),
        "no cross-tenant leak: {out}"
    );
    assert!(
        !out.contains(&bob_id.to_string()),
        "the failed id is not echoed: {out}"
    );
}

#[test]
fn full_returns_the_untruncated_body_while_the_default_truncates() {
    let memory = memory();
    let alice = Id::generate();
    let long = format!("HEAD_{}_TAIL", "x".repeat(2500));
    let id = seed(
        &memory,
        &long,
        Namespace::Agent(alice.to_string()),
        Role::Assistant,
    );

    // Default (no full, no verbose): the body is truncated to the snippet cap with an ellipsis,
    // so the far tail never appears.
    let truncated = read_memory_tool(&memory, read_params(&[id], alice), None, AuthEnabled(false))
        .expect("read");
    assert!(
        truncated.contains("..."),
        "default read truncates: {truncated}"
    );
    assert!(
        !truncated.contains("_TAIL"),
        "the tail is past the snippet cap: {truncated}"
    );

    // full=true: the entire body is returned, tail included, no ellipsis.
    let mut full = read_params(&[id], alice);
    full.full = Some(true);
    let out = read_memory_tool(&memory, full, None, AuthEnabled(false)).expect("read");
    assert!(out.contains("_TAIL"), "full returns the whole body: {out}");
    assert!(!out.contains("..."), "full does not truncate: {out}");
}

#[test]
fn a_single_id_read_is_just_requested_1_found_1() {
    let memory = memory();
    let alice = Id::generate();
    let id = seed(
        &memory,
        "the only memory",
        Namespace::Agent(alice.to_string()),
        Role::Assistant,
    );
    let out = read_memory_tool(&memory, read_params(&[id], alice), None, AuthEnabled(false))
        .expect("read");
    assert!(
        out.starts_with("[read_memory] requested=1 found=1"),
        "{out}"
    );
    assert!(out.contains("the only memory"), "{out}");
}

#[test]
fn a_repeated_id_is_read_once() {
    let memory = memory();
    let alice = Id::generate();
    let id = seed(
        &memory,
        "deduped memory",
        Namespace::Agent(alice.to_string()),
        Role::Assistant,
    );
    // The same id twice dedupes to a single request and a single found line.
    let out = read_memory_tool(
        &memory,
        read_params(&[id, id], alice),
        None,
        AuthEnabled(false),
    )
    .expect("read");
    assert!(
        out.starts_with("[read_memory] requested=1 found=1"),
        "{out}"
    );
    assert_eq!(
        out.matches("<memory ").count(),
        1,
        "deduped to one line: {out}"
    );
}

#[test]
fn empty_ids_oversized_ids_and_malformed_ids_are_call_level_errors() {
    let memory = memory();
    let alice = Id::generate();

    let empty = read_memory_tool(&memory, read_params(&[], alice), None, AuthEnabled(false))
        .expect_err("no ids is a call-level error");
    assert!(empty.starts_with("ERR_NO_MEMORY_IDS"), "{empty}");

    let too_many: Vec<Id> = (0..17).map(|_| Id::generate()).collect();
    let oversized = read_memory_tool(
        &memory,
        read_params(&too_many, alice),
        None,
        AuthEnabled(false),
    )
    .expect_err("more than 16 ids is a call-level error");
    assert!(oversized.starts_with("ERR_TOO_MANY_IDS"), "{oversized}");

    let mut malformed = read_params(&[], alice);
    malformed.memory_ids = vec!["not-a-uuid".to_string()];
    let bad = read_memory_tool(&memory, malformed, None, AuthEnabled(false))
        .expect_err("a non-uuid id is rejected");
    assert!(bad.starts_with("ERR_INVALID_MEMORY_ID"), "{bad}");
}

#[test]
fn a_system_role_memory_is_not_surfaced_by_default_even_when_requested() {
    let memory = memory();
    let alice = Id::generate();
    let id = seed(
        &memory,
        "a system directive turn",
        Namespace::Agent(alice.to_string()),
        Role::System,
    );

    // The request flag alone cannot surface system content: the default authority denies the
    // capability, so include_system=true still yields found=0 (a free bool is not a gate).
    let mut asked = read_params(&[id], alice);
    asked.include_system = Some(true);
    let out = read_memory_tool(&memory, asked, None, AuthEnabled(false)).expect("read");
    assert!(
        out.starts_with("[read_memory] requested=1 found=0"),
        "{out}"
    );
    assert!(!out.contains("a system directive turn"), "{out}");
}

#[test]
fn the_admin_capability_lifts_the_system_role_gate_only_when_the_caller_opts_in() {
    let admin = Id::generate();
    let memory = admin_memory(admin);
    let id = seed(
        &memory,
        "a privileged system directive",
        Namespace::Agent(admin.to_string()),
        Role::System,
    );

    // Capability granted AND the caller opts in -> the gate lifts, the system turn surfaces.
    let mut revealed = read_params(&[id], admin);
    revealed.include_system = Some(true);
    let lifted = read_memory_tool(&memory, revealed, None, AuthEnabled(false)).expect("read");
    assert!(
        lifted.starts_with("[read_memory] requested=1 found=1"),
        "{lifted}"
    );
    assert!(lifted.contains("a privileged system directive"), "{lifted}");

    // Same capability, but the caller does NOT opt in -> still hidden. Both halves of the AND
    // are required; the capability alone does not auto-surface system content.
    let hidden = read_memory_tool(&memory, read_params(&[id], admin), None, AuthEnabled(false))
        .expect("read");
    assert!(
        hidden.starts_with("[read_memory] requested=1 found=0"),
        "{hidden}"
    );
    assert!(
        !hidden.contains("a privileged system directive"),
        "{hidden}"
    );
}

#[test]
fn equivalent_uuid_spellings_dedupe_to_one_read() {
    let memory = memory();
    let alice = Id::generate();
    let id = seed(
        &memory,
        "single distinct memory",
        Namespace::Agent(alice.to_string()),
        Role::Assistant,
    );
    // The same memory addressed by two textually-distinct but equivalent UUID spellings
    // (canonical lowercase and uppercase) must collapse to one read — dedup keys on the parsed
    // Id, not the raw string, so neither the count nor a MAX_READ_IDS slot is double-charged.
    let mut params = read_params(&[id], alice);
    params.memory_ids = vec![id.to_string(), id.to_string().to_uppercase()];
    let out = read_memory_tool(&memory, params, None, AuthEnabled(false)).expect("read");
    assert!(
        out.starts_with("[read_memory] requested=1 found=1"),
        "equivalent spellings dedupe before the count: {out}"
    );
    assert_eq!(
        out.matches("<memory ").count(),
        1,
        "one line for the one distinct memory: {out}"
    );
}

#[test]
fn the_distinct_id_cap_is_measured_after_dedup() {
    let memory = memory();
    let alice = Id::generate();
    let ns = Namespace::Agent(alice.to_string());
    // 14 distinct memories, then pad the request to 20 raw ids by repeating six of them.
    let ids: Vec<Id> = (0..14)
        .map(|i| {
            seed(
                &memory,
                &format!("memory body {i}"),
                ns.clone(),
                Role::Assistant,
            )
        })
        .collect();
    let mut raw = ids.clone();
    raw.extend_from_slice(&ids[0..6]); // 20 raw ids -> 14 distinct after dedup

    // 20 raw ids that collapse to 14 distinct stay under the cap: dedup runs before the cap,
    // so the limit is measured on distinct memories rather than the raw request length.
    let out = read_memory_tool(&memory, read_params(&raw, alice), None, AuthEnabled(false))
        .expect("within the distinct-id cap");
    assert!(
        out.starts_with("[read_memory] requested=14 found=14"),
        "{out}"
    );
    assert_eq!(
        out.matches("<memory ").count(),
        14,
        "one line per distinct id: {out}"
    );

    // 17 DISTINCT ids exceed the cap regardless of duplicates.
    let seventeen: Vec<Id> = (0..17).map(|_| Id::generate()).collect();
    let oversized = read_memory_tool(
        &memory,
        read_params(&seventeen, alice),
        None,
        AuthEnabled(false),
    )
    .expect_err("17 distinct ids exceeds the cap");
    assert!(oversized.starts_with("ERR_TOO_MANY_IDS"), "{oversized}");
}

#[test]
fn verbose_widens_the_snippet_cap_between_default_and_full() {
    let memory = memory();
    let alice = Id::generate();
    // MID sits past the 240-char default cap but within the 2000-char verbose cap; TAIL sits
    // past the verbose cap so only full reveals it.
    let body = format!("HEAD_{}_MID_{}_TAIL", "x".repeat(300), "x".repeat(2000));
    let id = seed(
        &memory,
        &body,
        Namespace::Agent(alice.to_string()),
        Role::Assistant,
    );

    // Default: truncated before MID.
    let default = read_memory_tool(&memory, read_params(&[id], alice), None, AuthEnabled(false))
        .expect("read");
    assert!(
        !default.contains("_MID_"),
        "default truncates before MID: {default}"
    );

    // Verbose: MID is revealed, but TAIL past 2000 is still truncated with an ellipsis.
    let mut verbose = read_params(&[id], alice);
    verbose.verbose = Some(true);
    let out = read_memory_tool(&memory, verbose, None, AuthEnabled(false)).expect("read");
    assert!(
        out.contains("_MID_"),
        "verbose reveals content past the default cap: {out}"
    );
    assert!(
        !out.contains("_TAIL"),
        "verbose still truncates past 2000: {out}"
    );
    assert!(
        out.contains("..."),
        "verbose truncates the tail with an ellipsis: {out}"
    );

    // Full: the entire body, tail included.
    let mut full = read_params(&[id], alice);
    full.full = Some(true);
    let whole = read_memory_tool(&memory, full, None, AuthEnabled(false)).expect("read");
    assert!(whole.contains("_TAIL"), "full reveals the tail: {whole}");
}

#[test]
fn the_admin_reveal_lifts_the_system_namespace_gate_only_with_opt_in() {
    let admin = Id::generate();
    let memory = admin_memory(admin);
    // A system-role turn living in the system NAMESPACE (not the admin's own namespace). This
    // exercises the with_system() half of the reveal, which the role-gate tests never touch.
    let id = seed(
        &memory,
        "a system namespace directive",
        Namespace::System,
        Role::System,
    );

    // Admin capability AND opt-in: both the namespace gate (with_system) and the role gate lift.
    let mut revealed = read_params(&[id], admin);
    revealed.include_system = Some(true);
    let lifted = read_memory_tool(&memory, revealed, None, AuthEnabled(false)).expect("read");
    assert!(
        lifted.starts_with("[read_memory] requested=1 found=1"),
        "{lifted}"
    );
    assert!(lifted.contains("a system namespace directive"), "{lifted}");

    // Admin capability but NO opt-in: the system namespace stays excluded.
    let unopted = read_memory_tool(&memory, read_params(&[id], admin), None, AuthEnabled(false))
        .expect("read");
    assert!(
        unopted.starts_with("[read_memory] requested=1 found=0"),
        "{unopted}"
    );

    // A non-admin opting in cannot lift the system namespace gate.
    let outsider = Id::generate();
    let mut asked = read_params(&[id], outsider);
    asked.include_system = Some(true);
    let denied = read_memory_tool(&memory, asked, None, AuthEnabled(false)).expect("read");
    assert!(
        denied.starts_with("[read_memory] requested=1 found=0"),
        "{denied}"
    );
}
