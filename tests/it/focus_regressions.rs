//! Pinned public source + adversarial documents. No network or model required.
use donsetch::extract::{self, ExtractOptions, Extracted};

fn read(html: &str, query: Option<&str>, cap: usize) -> Extracted {
    extract::extract(
        html.as_bytes(),
        "text/html",
        "https://fixture.invalid/guide",
        &ExtractOptions {
            focus: query.map(str::to_owned),
            max_chars: Some(cap),
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn page_title_does_not_make_every_passage_relevant() {
    crate::sandbox();
    let mut html = String::from(
        "<html lang=en><title>Rust Guide</title><article><h1>Rust ownership guide</h1><p>Rust ownership transfers a value and prevents use after move.</p>",
    );
    for n in 0..20 {
        html.push_str(&format!("<h2>Chapter {n}</h2><p>Unrelated chapter {n} explains recipes, ingredients and preparation. Vegetables cook slowly in a covered pan until tender.</p>"));
    }
    html.push_str("</article></html>");
    let full = read(&html, None, 80_000);
    let focused = read(&html, Some("Rust ownership"), 80_000);
    assert!(focused.markdown.contains("prevents use after move"));
    assert!(
        !focused.markdown.contains("Vegetables"),
        "{}",
        focused.markdown
    );
    assert!(focused.markdown.len() * 3 < full.markdown.len());
    assert_eq!(focused.fingerprint, full.fingerprint);
}

#[test]
fn common_subject_does_not_outvote_the_requested_detail() {
    crate::sandbox();
    let mut html = String::from("<article><h1>AI Act</h1>");
    for n in 0..18 {
        html.push_str(&format!("<h2>AI policy {n}</h2><p>AI systems support regional innovation. This AI policy discusses incentives for companies and the regional economy in detail.</p>"));
    }
    html.push_str("<h2>Application timeline</h2><p>The AI Act deadline is 2 December 2027 for high-risk systems.</p><p>For regulated products, obligations apply on 2 August 2028.</p></article>");
    let ex = read(&html, Some("AI Act application timeline deadlines"), 2000);
    assert!(ex.markdown.contains("2 December 2027"), "{}", ex.markdown);
    assert!(ex.markdown.contains("2 August 2028"), "{}", ex.markdown);
    assert!(!ex.markdown.contains("regional economy"), "{}", ex.markdown);
    assert!(ex.next_offset.is_none());
}

#[test]
fn heading_match_does_not_swallow_a_long_mixed_section() {
    crate::sandbox();
    let mut html = String::from(
        "<article><h1>Rust Guide</h1><h2>Rust features</h2><p>Rust ownership moves values without copying their allocations.</p>",
    );
    for n in 0..18 {
        html.push_str(&format!("<p>Rust feature {n} covers syntax, formatting and developer tools. The compiler offers detailed explanations and useful diagnostics for these features.</p>"));
    }
    html.push_str("</article>");
    let ex = read(&html, Some("Rust ownership"), 2000);
    assert!(ex.markdown.contains("without copying"), "{}", ex.markdown);
    assert!(!ex.markdown.contains("Rust feature 17"));
    assert!(ex.next_offset.is_none());
}

#[test]
fn ec_ai_act_focus_keeps_requested_evidence_and_saves_tokens() {
    crate::sandbox();
    // European Commission source captured 2026-10-13:
    // https://digital-strategy.ec.europa.eu/en/policies/regulatory-framework-ai
    let html = include_str!("../fixtures/corpus/ec-ai-act.html");
    let full = read(html, None, 80_000);
    for (query, evidence) in [
        ("AI Act application timeline deadlines", "2 December 2027"),
        ("general purpose AI models obligations", "training content"),
    ] {
        let ex = read(html, Some(query), 80_000);
        assert!(ex.markdown.contains(evidence), "{query}: {}", ex.markdown);
        assert!(
            ex.markdown.len() * 100 <= full.markdown.len() * 65,
            "{query}: focused {} vs full {}: {}",
            ex.markdown.len(),
            full.markdown.len(),
            ex.markdown
        );
        assert!(!ex.markdown.contains("less than 20% saving"));
        assert_eq!(ex.fingerprint, full.fingerprint);
    }
}

#[test]
fn focus_miss_preserves_the_document_and_labels_the_miss() {
    crate::sandbox();
    let html = "<article><h1>Vegetable garden</h1><p>Carrots grow best in loose soil. Water regularly during dry periods and remove weeds carefully without disturbing the roots.</p></article>";
    let full = read(html, None, 80_000);
    let ex = read(html, Some("zzquasarzz"), 80_000);
    assert!(ex.markdown.contains("no matches"));
    assert!(ex.markdown.ends_with(&full.markdown));
}

#[test]
fn a_short_focus_slice_of_a_complete_page_is_not_a_js_shell() {
    crate::sandbox();
    let mut html = String::from(
        "<article><h1>Reference</h1><h2>Needle</h2><p>The exact needle is here: transactions survive power loss.</p><h2>Other material</h2>",
    );
    for n in 0..80 {
        html.push_str(&format!("<p>Unrelated paragraph {n} explains cooking, gardening and other hobbies with enough text to make this a complete and substantial source document.</p>"));
    }
    html.push_str("</article>");
    let ex = read(&html, Some("needle"), 2000);
    assert!(ex.markdown.contains("survive power loss"));
    assert!(
        !ex.thin,
        "successful focus is not evidence of a missing source: {}",
        ex.markdown
    );
    assert!(!ex.markdown.contains("likely JS-rendered"));
}

#[test]
fn duplicate_heading_names_do_not_import_unrelated_sections() {
    crate::sandbox();
    let html = "<article><h1>Guide</h1><h2>Alpha</h2><h3>Details</h3><p>The banana orchard produces fruit in late summer and needs regular irrigation.</p><h2>Beta</h2><h3>Details</h3><p>The ownership checker rejects access after a value has been moved elsewhere.</p><h2>Gamma</h2><p>Further gardening advice covers fertiliser and crop rotation for healthy vegetables.</p></article>";
    let ex = read(html, Some("ownership checker"), 2000);
    assert!(ex.markdown.contains("### Details"));
    assert!(ex.markdown.contains("## Beta"));
    assert!(!ex.markdown.contains("## Alpha"));
    assert!(!ex.markdown.contains("banana"));
    assert_eq!(
        ex.markdown.matches("### Details").count(),
        1,
        "{}",
        ex.markdown
    );
}

#[test]
fn focus_selects_table_rows_with_their_headers_and_cells_intact() {
    crate::sandbox();
    let mut html = String::from(
        "<article><h1>Service limits</h1><table><tr><th>Service</th><th>Retention policy</th></tr>",
    );
    for n in 0..60 {
        html.push_str(&format!("<tr><td>Background service {n}</td><td>Retains ordinary logs for thirty days.</td></tr>"));
    }
    html.push_str("<tr><td>Audit journal</td><td>Retention is seven years; legal holds suspend deletion.</td></tr></table></article>");
    let ex = read(&html, Some("audit journal retention"), 2000);
    assert!(
        ex.markdown.contains("legal holds suspend deletion"),
        "{}",
        ex.markdown
    );
    assert!(ex.markdown.contains("| Service | Retention policy |"));
    assert!(!ex.markdown.contains("Background service 59"));
    assert!(ex.next_offset.is_none());
    assert!(
        ex.markdown.contains("table rows"),
        "omissions must be disclosed"
    );
}

#[test]
fn focus_selects_unordered_items_without_losing_nested_conditions() {
    crate::sandbox();
    let mut html = String::from("<article><h1>Configuration reference</h1><ul>");
    for n in 0..60 {
        html.push_str(&format!(
            "<li>Setting {n} controls ordinary display preferences and interface formatting.</li>"
        ));
    }
    html.push_str("<li>Audit retention lasts seven years.<ul><li>Exception: legal holds suspend deletion.</li></ul></li></ul></article>");
    let ex = read(&html, Some("audit retention"), 2000);
    assert!(ex.markdown.contains("seven years"), "{}", ex.markdown);
    assert!(ex.markdown.contains("legal holds suspend deletion"));
    assert!(!ex.markdown.contains("Setting 59"));
    assert!(ex.next_offset.is_none());
    assert!(ex.markdown.contains("list items"));
}

#[test]
fn full_table_paginates_without_destroying_late_rows_or_long_cells() {
    crate::sandbox();
    let long_cell = format!(
        "{}The final condition is legally binding.",
        "Descriptive policy text. ".repeat(20)
    );
    let mut html =
        String::from("<article><h1>Policies</h1><table><tr><th>Rule</th><th>Details</th></tr>");
    for n in 0..80 {
        html.push_str(&format!(
            "<tr><td>Rule {n}</td><td>Ordinary guidance.</td></tr>"
        ));
    }
    html.push_str(&format!(
        "<tr><td>Audit</td><td>{long_cell}</td></tr></table></article>"
    ));
    let full = read(&html, None, 80000);
    assert!(full.markdown.contains("Rule 79"));
    assert!(full.markdown.contains(&long_cell));
    let ex = read(&html, Some("audit"), 2000);
    assert!(ex.markdown.contains(&long_cell));
    assert!(ex.markdown.contains("| Rule | Details |"));
    assert_eq!(ex.fingerprint, full.fingerprint);
    // must_contain reads the whole document, however small the window.
    let view = extract::view::View {
        must_contain: vec![extract::view::Term::parse("legally binding").unwrap()],
        max_chars: 2000,
        ..Default::default()
    };
    let probe = extract::view::render(&full.markdown, &view, None);
    assert_eq!(
        probe.verdict,
        Some(extract::view::Verdict::Match),
        "{}",
        probe.text
    );
}

#[test]
fn ordered_procedure_keeps_prerequisites_and_original_numbering() {
    crate::sandbox();
    let html = "<article><h1>Maintenance</h1><ol><li>Back up the database before making any changes.</li><li>Enable audit retention for seven years.</li><li>Verify the backup before restarting the service.</li></ol><h2>Gardening</h2><p>Water the vegetable garden regularly and rotate crops between growing seasons for good results.</p></article>";
    let ex = read(html, Some("audit retention"), 2000);
    assert!(ex.markdown.contains("1. Back up"));
    assert!(ex.markdown.contains("2. Enable audit"));
    assert!(ex.markdown.contains("3. Verify"));
}

#[test]
fn selecting_a_nested_condition_retains_its_parent_item() {
    crate::sandbox();
    let html = "<article><h1>Policies</h1><ul><li>Unrelated guidance covers gardening and crop rotation.</li><li>Audit retention lasts seven years.<ul><li>Exception: legal holds suspend deletion.</li><li>Apply this condition to pending litigation as well.</li></ul></li><li>Ordinary display settings control colours and font sizes.</li></ul></article>";
    let html = html.replace(
        "</ul></article>",
        &("<li>Extra unrelated display preferences and gardening guidance.</li>".repeat(10)
            + "</ul></article>"),
    );
    let ex = read(&html, Some("legal holds"), 2000);
    assert!(ex.markdown.contains("Audit retention lasts seven years"));
    assert!(ex.markdown.contains("pending litigation"));
    assert!(!ex.markdown.contains("crop rotation"));
}

#[test]
fn broad_focus_and_unicode_queries_keep_honest_contracts() {
    crate::sandbox();
    let broad = "<article><h1>Rust</h1><p>Rust ownership manages allocations safely and transfers values between functions without unnecessary copying.</p><p>Rust borrowing provides references with compile-time lifetime checks that protect memory from invalid access.</p></article>";
    let ex = read(broad, Some("Rust"), 2000);
    assert!(ex.markdown.contains("less than 20% saving"));
    assert!(ex.markdown.contains("ownership"));
    assert!(ex.markdown.contains("borrowing"));
    let html = "<html lang=zh><article><h1>机器学习指南</h1><h2>模型训练</h2><p>机器学习模型训练使用大量数据。训练完成后需要验证结果。</p><h2>日常生活</h2><p>今天天气很好，大家准备到公园散步和欣赏美丽的花朵。</p></article></html>";
    let html = html.replace("</article>", &format!("{}{}", "<p>关于日常生活的建议包括健康饮食、规律运动以及管理家庭花园。这些活动有助于维持良好的身体状态。</p>".repeat(12), "</article>"));
    let ex = read(&html, Some("模型训练"), 2000);
    assert!(ex.markdown.contains("验证结果"));
    assert!(!ex.markdown.contains("公园散步"));
}

#[test]
fn a_topic_read_retains_subsections_and_their_complete_enumerations() {
    crate::sandbox();
    let html = "<article><h1>Manual</h1><h2>Audit retention obligations</h2><p>Keep records for the required period.</p><h3>Supporting compliance</h3><ul><li>The public summary describes the retained records.</li><li>Publish the training content template with the summary.</li><li>Exceptions require a written legal justification.</li></ul><h2>Garden maintenance</h2><p>Regular watering and weeding keep vegetables healthy throughout the growing season. Rotate crops between seasons for best results.</p></article>";
    let html = html.replace("Regular watering and weeding", &("Further gardening guidance describes crop rotation, fertiliser, vegetable varieties, soil drainage and the proper growing season. ".repeat(8) + "Regular watering and weeding"));
    let ex = read(&html, Some("audit retention obligations"), 2000);
    for evidence in [
        "Supporting compliance",
        "training content template",
        "written legal justification",
    ] {
        assert!(ex.markdown.contains(evidence), "{}", ex.markdown);
    }
    assert!(!ex.markdown.contains("Regular watering"));
}

#[test]
fn python_taskgroup_exception_contract_survives_focus() {
    crate::sandbox();
    // https://docs.python.org/3/library/asyncio-task.html, 2026-10-13.
    let html = include_str!("../fixtures/corpus/python-asyncio.html");
    let ex = read(
        html,
        Some("Python asyncio TaskGroup exception handling"),
        80000,
    );
    for evidence in [
        "ExceptionGroup",
        "CancelledError",
        "KeyboardInterrupt",
        "SystemExit",
    ] {
        assert!(
            ex.markdown.contains(evidence),
            "missing {evidence}: {}",
            ex.markdown
        );
    }
    let full = read(html, None, 80000);
    assert!(ex.markdown.len() * 100 < full.markdown.len() * 75);
}

#[test]
fn later_table_cells_are_not_cut_to_the_first_rows_width() {
    crate::sandbox();
    let html = "<article><h1>Policies</h1><table><tr><th>Rule</th><th>Details</th></tr><tr><td>Ordinary</td><td>Keep logs.</td></tr><tr><td>Audit</td><td>Seven years.</td><td>Exception: legal holds suspend deletion.</td></tr></table></article>";
    let ex = read(html, None, 80000);
    assert!(
        ex.markdown
            .contains("Exception: legal holds suspend deletion")
    );
}
