//! Comprehensive integration tests for DonSift : the
//! extraction engine. Tests the full pipeline: HTML bytes in
//! → agent-ready markdown out. Every language, every block
//! type, every edge case. This is what makes DonSift
//! standalone-quality.

use super::language::{self, Script};
use super::metadata;
use super::*;
use scraper::Html;

#[test]
fn wave450_sitemap_is_an_inventory_not_an_app_shell() {
    let xml = b"<?xml version='1.0'?><urlset xmlns='http://www.sitemaps.org/schemas/sitemap/0.9'><url><loc>https://example.com/a?q=1&amp;p=2</loc></url><url><loc>https://example.com/b</loc></url></urlset>";
    let ex = extract(
        xml,
        "application/xml",
        "https://example.com/sitemap.xml",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(ex.markdown.contains("https://example.com/a?q=1&p=2"));
    assert!(!ex.markdown.contains("<urlset"));
    assert!(!ex.thin);
}

#[test]
fn wave450_focus_miss_keeps_its_warning() {
    let html = "<article><h1>Guide</h1><p>Reliable storage records each write before acknowledging the operation and safely restores interrupted transactions after a power failure.</p></article>";
    let ex = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("quasarzzzz".into()),
            ..Default::default()
        },
    );
    assert!(ex.markdown.contains("no matches"));
    assert!(ex.markdown.contains("Reliable storage"));
}

#[test]
fn wave450_pagination_never_skips_the_end_of_a_long_paragraph() {
    let text = format!("{}END-OF-PARAGRAPH\n\nNext paragraph.", "a".repeat(210));
    let (_, next) = paginate_public(&text, 0, 200);
    let (second, _) = paginate_public(&text, next.unwrap(), 200);
    assert!(
        second.contains("END-OF-PARAGRAPH"),
        "resumption skipped evidence: {second}"
    );
}

// ── Helpers ───────────────────────────────────────────────

fn extract_html(html: &str) -> Extracted {
    extract(
        html.as_bytes(),
        "text/html",
        "https://example.com/page",
        &ExtractOptions::default(),
    )
    .unwrap()
}

fn extract_html_opts(html: &str, opts: &ExtractOptions) -> Extracted {
    extract(
        html.as_bytes(),
        "text/html",
        "https://example.com/page",
        opts,
    )
    .unwrap()
}

// ════════════════════════════════════════════════════════════
// 1. MULTI-LANGUAGE EXTRACTION
// ════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════
// 0. PAGE IDENTITY vs REQUEST PARAMETERS
// ════════════════════════════════════════════════════════════

/// The history fingerprint must describe the PAGE, not the request.
/// It used to be taken over the rendered, focus-filtered slice with
/// the request's own notices prepended, so two reads of one unchanged
/// page with different reading parameters looked like a content
/// change: an agent alternating a focused and a full read of the same
/// URL got "changed"/"rewritten" on every call, and the parameters of
/// one call became the baseline for the next.
#[test]
fn fingerprint_describes_the_page_not_the_request() {
    // Enough paragraphs that a focus match has to drop some, which is
    // what emits the dropped-content manifest.
    let body: String = (0..12)
        .map(|i| format!("<p>Paragraph {i} about generic topics and filler text.</p>"))
        .collect();
    let html = format!(
        "<!DOCTYPE html><html><head><title>Identity</title></head><body>\
         <article><h1>Identity</h1>{body}\
         <p>The needle marker paragraph talks about rotating a deque.</p>\
         </article></body></html>"
    );

    let plain = extract_html_opts(&html, &ExtractOptions::default());
    let opts = ExtractOptions {
        focus: Some("needle marker".into()),
        ..Default::default()
    };
    let focused = extract_html_opts(&html, &opts);

    assert_ne!(
        plain.markdown, focused.markdown,
        "focus must still change what the caller sees"
    );
    assert_eq!(
        plain.fingerprint, focused.fingerprint,
        "the fingerprint must not move with the reading parameters"
    );
}

#[test]
fn extract_english_article() {
    let html = r#"<!DOCTYPE html>
<html lang="en"><head><title>Rust Programming Language</title>
<meta name="description" content="Rust is a systems programming language"></head>
<body>
<nav><a href="/">Home</a> | <a href="/blog">Blog</a></nav>
<article>
<h1>Rust Programming Language</h1>
<p>Rust is a systems programming language that runs blazingly fast,
prevents segfaults, and guarantees thread safety.</p>
<h2>Features</h2>
<p>Rust offers zero-cost abstractions, move semantics, and guaranteed
memory safety without a garbage collector.</p>
<h2>Memory Safety</h2>
<p>The borrow checker ensures references are valid, preventing
use-after-free and data races at compile time.</p>
</article>
<footer>Copyright 2026</footer>
</body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "en");
    assert!(!r.thin);
    assert!(
        r.markdown
            .contains("Rust is a systems programming language")
    );
    assert!(r.markdown.contains("borrow checker"));
    assert!(!r.markdown.contains("Home | Blog")); // nav stripped
    assert!(!r.markdown.contains("Copyright")); // footer stripped
    assert_eq!(r.title, Some("Rust Programming Language".to_string()));
    assert!(r.quality > 0.3);
}

#[test]
fn extract_chinese_article() {
    let html = r#"<html lang="zh-CN"><head><title>机器学习入门</title></head>
<body><article>
<h1>机器学习入门</h1>
<p>机器学习是人工智能的一个分支，它使计算机能够从数据中学习。</p>
<h2>监督学习</h2>
<p>监督学习使用标注数据训练模型，包括分类和回归两种类型。</p>
<h2>无监督学习</h2>
<p>无监督学习不需要标注数据，常用于聚类和降维等任务。</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "zh");
    assert!(r.markdown.contains("机器学习是人工智能"));
    assert!(r.markdown.contains("监督学习"));
    assert!(r.markdown.contains("无监督学习"));
    assert!(!r.thin);
}

#[test]
fn extract_japanese_article() {
    let html = r#"<html lang="ja"><head><title>機械学習とは</title></head>
<body><article>
<h1>機械学習とは</h1>
<p>機械学習は人工知能の一分野であり、データから学習する技術です。</p>
<h2>教師あり学習</h2>
<p>教師あり学習では、正解ラベル付きデータを使用してモデルを訓練します。</p>
<h2>深層学習</h2>
<p>深層学習はニューラルネットワークを用いた機械学習手法です。</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "ja");
    assert!(r.markdown.contains("機械学習は人工知能"));
    assert!(r.markdown.contains("深層学習"));
}

#[test]
fn extract_korean_article() {
    let html = r#"<html lang="ko"><head><title>머신러닝</title></head>
<body><article>
<h1>머신러닝</h1>
<p>머신러닝은 인공지능의 한 분야로 컴퓨터가 데이터로부터 학습하는 기술입니다.</p>
<h2>지도 학습</h2>
<p>지도 학습은 레이블이 있는 데이터를 사용하여 모델을 학습합니다.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "ko");
    assert!(r.markdown.contains("머신러닝은 인공지능"));
}

#[test]
fn extract_arabic_article() {
    let html = r#"<html lang="ar" dir="rtl"><head><title>تعلم الآلة</title></head>
<body><article>
<h1>تعلم الآلة</h1>
<p>تعلم الآلة هو فرع من الذكاء الاصطناعي يتيح للحواسيب التعلم من البيانات.</p>
<h2>التعلم الموجه</h2>
<p>يستخدم التعلم الموجه بيانات مصنفة لتدريب النماذج.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "ar");
    assert!(r.markdown.contains("تعلم الآلة"));
}

#[test]
fn extract_hindi_article() {
    let html = r#"<html lang="hi"><head><title>मशीन लर्निंग</title></head>
<body><article>
<h1>मशीन लर्निंग</h1>
<p>मशीन लर्निंग कृत्रिम बुद्धिमत्ता का एक भाग है जिसमें कंप्यूटर डेटा से सीखता है।</p>
<h2>पर्यवेक्षित अधिगम</h2>
<p>पर्यवेक्षित अधिगम में लेबल किए गए डेटा का उपयोग किया जाता है।</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "hi");
    assert!(r.markdown.contains("मशीन लर्निंग"));
}

#[test]
fn extract_nepali_article() {
    let html = r#"<html lang="ne"><head><title>मेसिन लर्निङ</title></head>
<body><article>
<h1>मेसिन लर्निङ</h1>
<p>मेसिन लर्निङ कृत्रिम बुद्धिमत्ताको एक शाखा हो जसमा कम्प्युटर डाटाबाट सिक्छ।</p>
<h2>निर्देशित सिकाइ</h2>
<p>निर्देशित सिकाइमा लेबल गरिएको डाटा प्रयोग गरिन्छ।</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "ne"); // html lang="ne" is present and respected
    assert!(r.markdown.contains("मेसिन लर्निङ"));
}

#[test]
fn extract_german_article() {
    let html = r#"<html lang="de"><head><title>Maschinelles Lernen</title></head>
<body><article>
<h1>Maschinelles Lernen</h1>
<p>Maschinelles Lernen ist ein Teilbereich der künstlichen Intelligenz.
Es ermöglicht Computern, aus Daten zu lernen.</p>
<h2>Überwachtes Lernen</h2>
<p>Beim überwachten Lernen werden markierte Daten verwendet,
um Modelle zu trainieren.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "de");
    assert!(r.markdown.contains("Maschinelles Lernen"));
    assert!(r.markdown.contains("künstlichen Intelligenz"));
}

#[test]
fn extract_french_article() {
    let html = r#"<html lang="fr"><head><title>L'apprentissage automatique</title></head>
<body><article>
<h1>L'apprentissage automatique</h1>
<p>L'apprentissage automatique est une branche de l'intelligence
artificielle qui permet aux ordinateurs d'apprendre à partir de données.</p>
<h2>Apprentissage supervisé</h2>
<p>L'apprentissage supervisé utilise des données étiquetées pour
entraîner des modèles.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "fr");
    assert!(r.markdown.contains("apprentissage automatique"));
}

#[test]
fn extract_spanish_article() {
    let html = r#"<html lang="es"><head><title>Aprendizaje Automático</title></head>
<body><article>
<h1>Aprendizaje Automático</h1>
<p>El aprendizaje automático es una rama de la inteligencia artificial
que permite a las computadoras aprender de los datos.</p>
<h2>Aprendizaje Supervisado</h2>
<p>El aprendizaje supervisado utiliza datos etiquetados para entrenar modelos.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "es");
    assert!(r.markdown.contains("aprendizaje automático"));
}

#[test]
fn extract_russian_article() {
    let html = r#"<html lang="ru"><head><title>Машинное обучение</title></head>
<body><article>
<h1>Машинное обучение</h1>
<p>Машинное обучение : это раздел искусственного интеллекта,
который позволяет компьютерам учиться на данных.</p>
<h2>Контролируемое обучение</h2>
<p>При контролируемом обучении используются размеченные данные
для обучения моделей.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "ru");
    assert!(r.markdown.contains("Машинное обучение"));
}

#[test]
fn extract_thai_article() {
    let html = r#"<html lang="th"><head><title>การเรียนรู้ของเครื่อง</title></head>
<body><article>
<h1>การเรียนรู้ของเครื่อง</h1>
<p>การเรียนรู้ของเครื่องเป็นสาขาหนึ่งของปัญญาประดิษฐ์</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "th");
    assert!(r.markdown.contains("การเรียนรู้"));
}

// ════════════════════════════════════════════════════════════
// 2. LANGUAGE DETECTION
// ════════════════════════════════════════════════════════════

#[test]
fn lang_detect_from_html_attr() {
    let doc = Html::parse_document(r#"<html lang="zh-Hant"><body>內容</body></html>"#);
    let info = language::detect(&doc);
    assert_eq!(info.code, "zh");
    assert_eq!(info.script, Script::Han);
}

#[test]
fn lang_detect_from_meta() {
    let doc = Html::parse_document(
        r#"<html><head>
<meta http-equiv="content-language" content="ja"></head>
<body>内容</body></html>"#,
    );
    let info = language::detect(&doc);
    assert_eq!(info.code, "ja");
}

#[test]
fn lang_detect_from_script_analysis() {
    let doc = Html::parse_document(
        "<html><body>한국어 텍스트입니다. 이것은 기계 학습에 대한 글입니다.</body></html>",
    );
    let info = language::detect(&doc);
    // Undeclared: the script is observed, the language is not guessed.
    assert_eq!(info.code, "unknown");
    assert_eq!(info.source, None);
    assert_eq!(info.script, Script::Hangul);
}

#[test]
fn lang_detect_does_not_guess_from_script() {
    // No declaration: the language is unknown. Script (Latin here) is
    // observed, and drives tokenization, but does not name a language.
    let doc = Html::parse_document("<html><body>Some plain English prose.</body></html>");
    let info = language::detect(&doc);
    assert_eq!(info.code, "unknown");
    assert_eq!(info.source, None);
    assert_eq!(info.script, Script::Latin);
}

#[test]
fn lang_detect_mixed_japanese() {
    let doc = Html::parse_document(
        "<html><body>これは日本語の文章です。機械学習について説明します。</body></html>",
    );
    let info = language::detect(&doc);
    // Undeclared: Kana/Han are observed (and drive tokenization), the
    // language itself is not inferred from them.
    assert_eq!(info.code, "unknown");
    assert!(info.scripts.contains(&Script::Kana));
    assert!(info.scripts.contains(&Script::Han));
}

// ════════════════════════════════════════════════════════════
// 3. FOCUS FILTERING ACROSS LANGUAGES
// ════════════════════════════════════════════════════════════

#[test]
fn focus_english_stemming() {
    let html = r#"<html lang="en"><body><article>
<h1>Running Guide</h1>
<p>The runner was running quickly through the park. Running is excellent exercise.</p>
<p>Cooking is also a great hobby. Many people enjoy cooking Italian food.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("run".to_string()),
            ..Default::default()
        },
    );
    assert!(!r.markdown.contains("no matches"), "{}", r.markdown);
    assert!(r.markdown.contains("running") || r.markdown.contains("runner"));
}

#[test]
fn focus_chinese_bigram() {
    let html = r#"<html lang="zh"><body><article>
<h1>机器学习入门</h1>
<p>机器学习是人工智能的一个重要分支。</p>
<p>今天天气很好，适合出门散步。</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("机器学习".to_string()),
            ..Default::default()
        },
    );
    assert!(!r.markdown.contains("no matches"), "{}", r.markdown);
    assert!(r.markdown.contains("机器学习"));
}

#[test]
fn focus_japanese() {
    let html = r#"<html lang="ja"><body><article>
<h1>機械学習</h1>
<p>機械学習は人工知能の一分野である。</p>
<p>今日はいい天気ですね。</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("機械学習".to_string()),
            ..Default::default()
        },
    );
    assert!(!r.markdown.contains("no matches"), "{}", r.markdown);
    assert!(r.markdown.contains("機械学習"));
}

#[test]
fn focus_accent_folding() {
    let html = r#"<html lang="fr"><body><article>
<p>Le café est une boisson très populaire en France. Les cafés parisients sont célèbres.</p>
<p>Le thé est aussi apprécié par beaucoup de gens.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("cafe".to_string()),
            ..Default::default()
        },
    );
    assert!(!r.markdown.contains("no matches"), "{}", r.markdown);
}

#[test]
fn focus_german_umlaut() {
    let html = r#"<html lang="de"><body><article>
<p>Die Universität bietet viele Kurse an. Die Universitäten in Deutschland sind bekannt.</p>
<p>Das Wetter ist heute schön.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("Universität".to_string()),
            ..Default::default()
        },
    );
    // Should find the content regardless of umlaut folding.
    assert!(!r.markdown.contains("no matches"), "{}", r.markdown);
}

#[test]
fn focus_miss_returns_full_content() {
    let html = r#"<html><body><article>
<p>The quick brown fox jumps over the lazy dog.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("quantum entanglement".to_string()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("*[focus"));
    assert!(r.markdown.contains("quick brown fox"));
}

// ════════════════════════════════════════════════════════════
// 4. METADATA EXTRACTION
// ════════════════════════════════════════════════════════════

#[test]
fn metadata_og_title() {
    let html = r#"<html><head>
<meta property="og:title" content="The Real Title">
<title>Something Else</title>
</head><body><p>Content</p></body></html>"#;
    let doc = Html::parse_document(html);
    let meta = metadata::metadata(&doc);
    assert_eq!(meta.title, Some("The Real Title".to_string()));
}

#[test]
fn metadata_description() {
    let html = r#"<html><head>
<meta name="description" content="A comprehensive guide to Rust programming.">
</head><body><p>Content</p></body></html>"#;
    let doc = Html::parse_document(html);
    let meta = metadata::metadata(&doc);
    assert_eq!(
        meta.description.as_deref(),
        Some("A comprehensive guide to Rust programming.")
    );
}

#[test]
fn metadata_canonical() {
    let html = r#"<html><head>
<link rel="canonical" href="https://example.com/canonical-page">
</head><body><p>Content</p></body></html>"#;
    let doc = Html::parse_document(html);
    let meta = metadata::metadata(&doc);
    assert_eq!(
        meta.canonical.as_deref(),
        Some("https://example.com/canonical-page")
    );
}

#[test]
fn metadata_published_time() {
    let html = r#"<html><head>
<meta property="article:published_time" content="2026-01-15T10:30:00Z">
</head><body><p>Content</p></body></html>"#;
    let doc = Html::parse_document(html);
    let meta = metadata::metadata(&doc);
    assert_eq!(meta.published.as_deref(), Some("2026-01-15"));
}

#[test]
fn metadata_json_ld_author() {
    let html = r#"<html><head>
<script type="application/ld+json">
{"@type":"Article","author":{"name":"Jane Doe"},"datePublished":"2026-02-01"}
</script>
</head><body><p>Content</p></body></html>"#;
    let doc = Html::parse_document(html);
    let meta = metadata::metadata(&doc);
    assert_eq!(meta.byline.as_deref(), Some("Jane Doe"));
}

#[test]
fn metadata_description_in_frontmatter() {
    let html = r#"<html><head>
<meta name="description" content="A short summary.">
</head><body><article>
<h1>Test Page</h1>
<p>Some content here that is long enough to be extracted.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("> A short summary."));
}

// ── <base href> ───────────────────────────────────────────

const BASE_PAD: &str = "<p>This paragraph is ordinary article prose that exists only so the scorer sees a real article body with enough text, punctuation, and paragraphs to win.</p><p>A second distinct paragraph of filler prose, again present only so that the article container clearly wins the main-content scoring pass.</p>";

// An article at https://example.com/a/b.html, links on, whose <head>
// carries `head`: a path-relative, a root-relative and an absolute
// link, and one content image with a relative src.
fn extract_with_base(head: &str) -> Extracted {
    extract_with_base_at(head, "https://example.com/a/b.html")
}

// The same article, extracted as if fetched from `page_url`.
fn extract_with_base_at(head: &str, page_url: &str) -> Extracted {
    let html = format!(
        "<!doctype html><html><head>{head}<title>Base</title></head><body><article><h1>Base</h1>{BASE_PAD}<p>Read the <a href=\"docs/install\">installation guide</a>, the <a href=\"/abs/path\">reference</a> and the <a href=\"https://other.example.net/faq\">FAQ</a>.</p><img src=\"images/diagram.png\" alt=\"System diagram\" width=\"600\" height=\"400\"></article></body></html>"
    );
    extract(
        html.as_bytes(),
        "text/html",
        page_url,
        &ExtractOptions {
            include_links: true,
            ..Default::default()
        },
    )
    .unwrap()
}

fn has_image(ex: &Extracted, src: &str) -> bool {
    ex.images.iter().any(|(_, s)| s == src)
}

// A relative <base href> ("/" is Angular's default) resolves against
// the document URL (HTML §4.2.3). Taken raw it is not a parsable base,
// so every relative link and image was discarded.
#[test]
fn relative_base_href_resolves_against_the_page() {
    let html = format!(
        "<!doctype html><html><head><base href=\"/\"><title>Base</title></head><body><article><h1>Base</h1>{BASE_PAD}<p>Read the <a href=\"docs/install\">installation guide</a> first.</p><img src=\"images/diagram.png\" alt=\"System diagram\" width=\"600\" height=\"400\"></article></body></html>"
    );
    let ex = extract(
        html.as_bytes(),
        "text/html",
        "https://example.com/a/b.html",
        &ExtractOptions {
            include_links: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        ex.markdown
            .contains("[installation guide](https://example.com/docs/install)"),
        "{}",
        ex.markdown
    );
    assert!(
        ex.images
            .iter()
            .any(|(_, src)| src == "https://example.com/images/diagram.png"),
        "images: {:?}",
        ex.images
    );
}

// A base under a path: relative links and images resolve under it, a
// root-relative link resolves to the host root, an absolute link is
// untouched.
#[test]
fn a_base_href_under_a_path_resolves_every_relative_form() {
    let ex = extract_with_base("<base href=\"/guide/\">");
    let md = &ex.markdown;
    for link in [
        "[installation guide](https://example.com/guide/docs/install)",
        "[reference](https://example.com/abs/path)",
        "[FAQ](https://other.example.net/faq)",
    ] {
        assert!(md.contains(link), "missing {link}:\n{md}");
    }
    assert!(
        has_image(&ex, "https://example.com/guide/images/diagram.png"),
        "images: {:?}",
        ex.images
    );
}

// A javascript: or data: base falls back to the page URL, as HTML's
// "set the frozen base URL" step does (§4.2.3): the links resolve as
// if the element were absent.
#[test]
fn a_non_http_base_href_falls_back_to_the_page() {
    for base in ["javascript:void(0)", "data:text/html,x"] {
        let ex = extract_with_base(&format!("<base href=\"{base}\">"));
        let md = &ex.markdown;
        for link in [
            "[installation guide](https://example.com/a/docs/install)",
            "[reference](https://example.com/abs/path)",
        ] {
            assert!(md.contains(link), "base {base}: missing {link}:\n{md}");
        }
        assert!(
            has_image(&ex, "https://example.com/a/images/diagram.png"),
            "base {base}: images: {:?}",
            ex.images
        );
    }
}

// A file: base would resolve every relative link (a browser adopts
// it), but only an http(s) base is adopted: it is dropped and the page
// URL is the base.
#[test]
fn a_file_base_href_is_never_adopted() {
    let ex = extract_with_base("<base href=\"file:///srv/site/\">");
    let md = &ex.markdown;
    assert!(!md.contains("](file:"), "{md}");
    assert!(
        md.contains("[installation guide](https://example.com/a/docs/install)"),
        "{md}"
    );
    assert!(
        has_image(&ex, "https://example.com/a/images/diagram.png"),
        "images: {:?}",
        ex.images
    );
}

// A page URL that does not parse leaves nothing to resolve a relative
// base against, but an absolute http(s) base still carries the links;
// a file: one still does not.
#[test]
fn an_absolute_base_href_works_without_a_parsable_page_url() {
    let ex = extract_with_base_at("<base href=\"https://cdn.example.org/root/\">", "not a url");
    let md = &ex.markdown;
    for link in [
        "[installation guide](https://cdn.example.org/root/docs/install)",
        "[reference](https://cdn.example.org/abs/path)",
    ] {
        assert!(md.contains(link), "missing {link}:\n{md}");
    }
    assert!(
        has_image(&ex, "https://cdn.example.org/root/images/diagram.png"),
        "images: {:?}",
        ex.images
    );

    let ex = extract_with_base_at("<base href=\"file:///srv/site/\">", "not a url");
    assert!(!ex.markdown.contains("](file:"), "{}", ex.markdown);
    assert!(
        ex.images.iter().all(|(_, s)| !s.starts_with("file:")),
        "images: {:?}",
        ex.images
    );
}

// Negative: an absolute base still replaces the page URL.
#[test]
fn an_absolute_base_href_still_wins() {
    let ex = extract_with_base("<base href=\"https://cdn.example.org/root/\">");
    let md = &ex.markdown;
    for link in [
        "[installation guide](https://cdn.example.org/root/docs/install)",
        "[reference](https://cdn.example.org/abs/path)",
        "[FAQ](https://other.example.net/faq)",
    ] {
        assert!(md.contains(link), "missing {link}:\n{md}");
    }
    assert!(!md.contains("](https://example.com/"), "{md}");
    assert!(
        has_image(&ex, "https://cdn.example.org/root/images/diagram.png"),
        "images: {:?}",
        ex.images
    );
}

// Negative: without a base, links resolve against the page itself.
#[test]
fn a_page_without_base_href_resolves_against_itself() {
    let ex = extract_with_base("");
    let md = &ex.markdown;
    for link in [
        "[installation guide](https://example.com/a/docs/install)",
        "[reference](https://example.com/abs/path)",
        "[FAQ](https://other.example.net/faq)",
    ] {
        assert!(md.contains(link), "missing {link}:\n{md}");
    }
    assert!(
        has_image(&ex, "https://example.com/a/images/diagram.png"),
        "images: {:?}",
        ex.images
    );
}

// ════════════════════════════════════════════════════════════
// 5. BLOCK TYPES
// ════════════════════════════════════════════════════════════

#[test]
fn block_table_extraction() {
    let html = r#"<html><body><article>
<table>
<tr><th>Name</th><th>Value</th><th>Description</th></tr>
<tr><td>Rust</td><td>1.75</td><td>Systems language</td></tr>
<tr><td>Go</td><td>1.22</td><td>Simple concurrency</td></tr>
</table>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("| Name | Value |"));
    assert!(r.markdown.contains("| Rust | 1.75 |"));
    assert!(r.markdown.contains("| Go | 1.22 |"));
}

#[test]
fn block_code_pre() {
    let html = r#"<html><body><article>
<pre><code class="language-rust">fn main() {
    println!("Hello, world!");
}</code></pre>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("```rust"));
    assert!(r.markdown.contains("fn main()"));
    assert!(r.markdown.contains("println!"));
}

// A <pre> that itself contains a markdown fence (every "how to
// write markdown" page, every README rendered in a docs site)
// used to be wrapped in a bare ``` fence: the inner ``` closed
// the block early and the rest of the code spilled out as prose.
#[test]
fn block_code_containing_backtick_fence_is_wrapped_in_a_longer_fence() {
    let html = "<html><body><article>
<pre><code>Use a fence:

```rust
fn main() {}
```

Then prose.</code></pre>
</article></body></html>";
    let r = extract_html(html);
    let md = &r.markdown;
    let open = md.find("````\n").expect("longer opening fence");
    let close = md.rfind("\n````\n").expect("longer closing fence");
    assert!(open < close);
    let inside = &md[open..close];
    assert!(inside.contains("```rust\nfn main() {}\n```"), "{md}");
    assert!(inside.contains("Then prose."), "{md}");
}

#[test]
fn block_code_whitespace_preserved() {
    let html = r#"<html><body><article>
<pre>line one
  line two
    line three</pre>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("  line two"));
    assert!(r.markdown.contains("    line three"));
}

#[test]
fn block_quote() {
    let html = r#"<html><body><article>
<blockquote>To be or not to be, that is the question.</blockquote>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("> To be or not to be"));
}

#[test]
fn block_ordered_list() {
    let html = r#"<html><body><article>
<ol>
<li>First step</li>
<li>Second step</li>
<li>Third step</li>
</ol>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("1. First step"));
    assert!(r.markdown.contains("2. Second step"));
    assert!(r.markdown.contains("3. Third step"));
}

#[test]
fn block_unordered_list() {
    let html = r#"<html><body><article>
<ul>
<li>Apple</li>
<li>Banana</li>
<li>Cherry</li>
</ul>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("- Apple"));
    assert!(r.markdown.contains("- Banana"));
    assert!(r.markdown.contains("- Cherry"));
}

#[test]
fn block_nested_list() {
    let html = r#"<html><body><article>
<ul>
<li>Top level</li>
<li>Has nested
  <ul>
    <li>Nested item</li>
  </ul>
</li>
</ul>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("Top level"));
    assert!(r.markdown.contains("Nested item"));
}

// The outer <li>'s inline render used to descend into the nested
// <ul> as well, so every nested item appeared twice: fused into
// its parent's line ("Has nested Nested item") and again as its
// own indented bullet.
#[test]
fn block_nested_list_items_are_not_duplicated() {
    let html = r#"<html><body><article>
<p>Intro paragraph long enough to keep the article.</p>
<ul>
<li>Top level</li>
<li>Has nested
  <ul>
    <li>Nested item</li>
    <li>Second nested</li>
  </ul>
</li>
<li>Last</li>
</ul>
</article></body></html>"#;
    let r = extract_html(html);
    let md = &r.markdown;
    assert_eq!(md.matches("Nested item").count(), 1, "{md}");
    assert_eq!(md.matches("Second nested").count(), 1, "{md}");
    assert!(
        md.contains("- Has nested\n  - Nested item\n  - Second nested\n- Last"),
        "{md}"
    );
}

// Past the indentation cap the nested list is flattened into its
// parent's line (as before), never dropped; and text on either
// side of a nested list keeps its word boundary.
#[test]
fn block_nested_list_deep_levels_and_trailing_text_survive() {
    let html = r#"<html><body><article>
<p>Intro paragraph long enough to keep the article.</p>
<ul><li>L0<ul><li>L1<ul><li>L2<ul><li>L3<ul><li>L4<ul><li>L5 deep text<ul><li>L6 deeper</li></ul></li></ul></li></ul></li></ul></li></ul></li></ul></li>
<li>Before<ul><li>Inner</li></ul>After text</li>
</ul>
</article></body></html>"#;
    let r = extract_html(html);
    let md = &r.markdown;
    assert!(md.contains("        - L4 L5 deep text L6 deeper\n"), "{md}");
    assert_eq!(md.matches("deep text").count(), 1, "{md}");
    assert!(md.contains("- Before After text\n  - Inner\n"), "{md}");
}

#[test]
fn block_definition_list() {
    let html = r#"<html><body><article>
<dl>
<dt>Rust</dt>
<dd>A systems programming language focused on safety.</dd>
<dt>Go</dt>
<dd>A simple language with great concurrency support.</dd>
</dl>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("**Rust**"));
    assert!(r.markdown.contains("systems programming language"));
    assert!(r.markdown.contains("**Go**"));
    assert!(r.markdown.contains("concurrency support"));
}

#[test]
fn block_table_pipe_escaped() {
    let html = r#"<html><body><article>
<table>
<tr><th>Expr</th><th>Result</th></tr>
<tr><td>a | b</td><td>valid</td></tr>
<tr><td>c | d</td><td>done</td></tr>
<tr><td>e | f</td><td>ok</td></tr>
</table>
</article></body></html>"#;
    let r = extract_html(html);
    // Pipe should be escaped to not break markdown table.
    assert!(r.markdown.contains("a \\| b"));
}

#[test]
fn block_links_stripped_by_default() {
    let html = r#"<html><body><article>
<p>Read the <a href="https://rust-lang.org/docs">documentation</a> for more info.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("documentation"));
    assert!(!r.markdown.contains("[documentation]"));
    assert!(!r.markdown.contains("rust-lang.org"));
}

#[test]
fn block_links_kept_with_option() {
    let html = r#"<html><body><article>
<p>Read the <a href="https://rust-lang.org/docs">documentation</a> for more info.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            include_links: true,
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("[documentation]"));
    assert!(r.markdown.contains("rust-lang.org"));
}

#[test]
fn block_link_trackers_stripped() {
    let html = r#"<html><body><article>
<p>Check <a href="https://example.com/page?utm_source=newsletter&utm_medium=email&fbclid=abc123">this link</a> out.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            include_links: true,
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("[this link]"));
    assert!(!r.markdown.contains("utm_source"));
    assert!(!r.markdown.contains("fbclid"));
}

// ════════════════════════════════════════════════════════════
// 6. EDGE CASES
// ════════════════════════════════════════════════════════════

#[test]
fn edge_empty_page() {
    let html = r#"<html><head></head><body></body></html>"#;
    let r = extract_html(html);
    assert!(r.thin || r.markdown.contains("no extractable content"));
}

#[test]
fn edge_malformed_html() {
    let html = r#"<html><body><div><p>Unclosed paragraph<div>More text</div></p></div><span>Loose text</span>"#;
    let r = extract_html(html);
    // Should not crash and should extract something.
    assert!(r.markdown.contains("Unclosed paragraph") || r.markdown.contains("More text"));
}

#[test]
fn edge_deeply_nested() {
    let mut html = String::from("<html><body><article>");
    for _ in 0..100 {
        html.push_str("<div><p>Content</p>");
    }
    html.push_str("Deep text");
    for _ in 0..100 {
        html.push_str("</div>");
    }
    html.push_str("</article></body></html>");
    let r = extract_html(&html);
    assert!(r.markdown.contains("Content") || r.markdown.contains("Deep text"));
    // Should not hang or crash.
}

#[test]
fn edge_huge_page() {
    let mut html = String::from("<html><body><article><h1>Big Page</h1>");
    for i in 0..500 {
        html.push_str(&format!(
            "<p>Paragraph number {i} with some text content here.</p>"
        ));
    }
    html.push_str("</article></body></html>");
    let r = extract_html(&html);
    assert!(r.markdown.contains("Big Page"));
    assert!(r.blocks_total > 100);
}

#[test]
fn edge_tiny_page() {
    let html = r#"<html><body><p>Hi</p></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("Hi"));
}

#[test]
fn edge_non_html_passthrough_json() {
    let json = br#"{"name": "test", "value": 42}"#;
    let r = extract(
        json,
        "application/json",
        "https://example.com/api",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(r.markdown.contains("test"));
    assert_eq!(r.content_kind, ContentKind::Page);
}

#[test]
fn edge_non_html_passthrough_text() {
    let r = extract(
        b"Hello, plain text world!",
        "text/plain",
        "https://example.com/txt",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(r.markdown.contains("Hello, plain text"));
}

#[test]
fn edge_no_html_tag() {
    let html = r#"<div><p>Content without html tag</p></div>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("Content without html"));
}

#[test]
fn edge_only_nav_and_footer() {
    let html = r#"<html><body>
<nav><a href="/">Home</a><a href="/about">About</a></nav>
<footer>© 2026 Site. All rights reserved.</footer>
</body></html>"#;
    let r = extract_html(html);
    // Should have minimal content : nav and footer are junk.
    assert!(r.blocks_shown < 5);
}

#[test]
fn edge_div_soup() {
    // No semantic tags : just divs with text.
    let html = r#"<html><body>
<div class="wrapper">
<div class="content">This is the main content paragraph with enough text to be extracted properly.</div>
<div class="sidebar">Sidebar junk that should be ignored.</div>
</div>
</body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("main content paragraph"));
}

#[test]
fn edge_whitespace_only() {
    let html = "<html><body>   \n\n\n   </body></html>";
    let r = extract_html(html);
    // Should not crash; content should be minimal/empty.
    assert!(
        r.thin
            || r.markdown.contains("no extractable")
            || r.markdown.trim().is_empty()
            || r.blocks_shown == 0
    );
}

// ════════════════════════════════════════════════════════════
// 7. PAGINATION
// ════════════════════════════════════════════════════════════

#[test]
fn pagination_basic() {
    let mut html = String::from("<html><body><article><h1>Page</h1>");
    for i in 0..100 {
        html.push_str(&format!(
            "<p>Paragraph {i}: Lorem ipsum dolor sit amet consectetur adipiscing elit.</p>"
        ));
    }
    html.push_str("</article></body></html>");
    let r = extract_html_opts(
        &html,
        &ExtractOptions {
            max_chars: Some(500),
            ..Default::default()
        },
    );
    assert!(r.next_offset.is_some());
    assert!(r.markdown.contains("truncated"));
    assert!(r.markdown.len() < 700); // near 500 + truncation marker
}

#[test]
fn pagination_resume() {
    let mut html = String::from("<html><body><article><h1>Page</h1>");
    for i in 0..100 {
        html.push_str(&format!(
            "<p>Paragraph {i}: Lorem ipsum dolor sit amet consectetur adipiscing elit.</p>"
        ));
    }
    html.push_str("</article></body></html>");
    let r1 = extract_html_opts(
        &html,
        &ExtractOptions {
            max_chars: Some(500),
            ..Default::default()
        },
    );
    let offset = r1.next_offset.unwrap();
    let r2 = extract_html_opts(
        &html,
        &ExtractOptions {
            max_chars: Some(500),
            offset,
            ..Default::default()
        },
    );
    // Page 2 should have different content than page 1.
    assert_ne!(r1.markdown, r2.markdown);
}

#[test]
fn pagination_utf8_boundary_safe() {
    // Build content with multibyte chars.
    let html = r#"<html><body><article>
<p>日本語のテキストです。機械学習について説明しています。この文章は十分な長さが必要です。</p>
<p>さらにテキストを追加します。これは二番目の段落です。テキストの長さが文字境界を超えることを確認します。</p>
<p>三番目の段落です。これは最後の段落になります。十分な長さがあることを確認してください。</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            max_chars: Some(100),
            ..Default::default()
        },
    );
    // Should not panic on UTF-8 boundaries.
    assert!(!r.markdown.is_empty());
    // The slice should be valid UTF-8 (implicit : if it wasn't, the String would be invalid).
}

#[test]
fn pagination_offset_past_end() {
    let html = r#"<html><body><article><p>Short content.</p></article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            offset: 10000,
            ..Default::default()
        },
    );
    assert!(r.markdown.is_empty());
    assert!(r.next_offset.is_none());
}

// ════════════════════════════════════════════════════════════
// 8. TOC MODE
// ════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════
// 9. SECTION MODE
// ════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════
// 10. CONTENT CLASSIFICATION
// ════════════════════════════════════════════════════════════

#[test]
fn classify_article() {
    let html = r#"<html><body><article>
<h1>Article Title</h1>
<p>A substantial paragraph with enough text to be considered real content for proper article classification. This paragraph is long enough to meet the minimum threshold for article classification in the content type detection system.</p>
<h2>Section One</h2>
<p>Another substantial paragraph that continues the article with meaningful discussion. This one also has enough text to be meaningful and contribute to the article classification score with sufficient length.</p>
<h2>Section Two</h2>
<p>Yet another paragraph in this article discussing important topics in depth. The article has multiple headings and substantial paragraphs, which should classify it as an Article with structured prose content.</p>
<h2>Section Three</h2>
<p>The final paragraph of this article providing concluding remarks. With multiple headings and substantial prose throughout, this should definitely classify as Article content type in the classification system.</p>
<h2>Section Four</h2>
<p>An additional paragraph to meet the five-paragraph minimum for article classification with sufficient heading structure and enough text length per paragraph to pass all thresholds.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.content_kind, ContentKind::Article);
}

#[test]
fn classify_docs() {
    let html = r#"<html><body><article>
<h1>API Reference</h1>
<p>Description of the API.</p>
<pre><code>fetch(url).then(r => r.json())</code></pre>
<pre><code>const data = await getData()</code></pre>
<pre><code>console.log(result)</code></pre>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.content_kind, ContentKind::Docs);
}

// ════════════════════════════════════════════════════════════
// 11. CONTENT DENSITY SHELL DETECTION (v2.3.0)
// ════════════════════════════════════════════════════════════

/// A large page (> 20KB) that yields < 5% of its size as text
/// with < 3000 chars total is a JS shell. SPAs that server-render
/// their layout (navigation, sidebar, footer) produce enough
/// boilerplate to pass the < 800 char thin check, but the main
/// content is client-rendered. Content density catches these.
#[test]
fn content_density_shell_large_page_low_density_is_thin() {
    // 60KB of HTML with only ~900 chars of visible text.
    // Simulates a SPA shell with navigation + noscript + sidebar.
    let mut html = String::from("<html><head><script>");
    // Pad with JS to reach > 20KB
    html.push_str(&"var x = 1;".repeat(3000));
    html.push_str("</script></head><body>");
    html.push_str("<nav>Home About Contact Login Search Settings</nav>");
    html.push_str("<noscript>Enable JavaScript for the best experience</noscript>");
    html.push_str("<div id=\"root\"></div>");
    html.push_str("<footer>Copyright 2026 Terms Privacy Cookies</footer>");
    html.push_str("</body></html>");
    assert!(
        html.len() > 20_000,
        "test HTML must be > 20KB, got {}",
        html.len()
    );
    let r = extract_html(&html);
    assert!(
        r.thin,
        "large page with < 5% content density must be thin (shell), got density {:.3}",
        r.total_chars as f64 / html.len() as f64
    );
}

/// A large page with high content density is NOT thin.
/// Real articles (rust-lang docs, softwaremill blog) have 15-40%+ density.
#[test]
fn content_density_real_page_high_density_not_thin() {
    // 60KB+ of HTML with ~30KB of real article text (50% density).
    let mut html = String::from("<html><body><article>");
    html.push_str("<h1>Rust Static vs Dynamic Dispatch</h1>");
    for i in 0..200 {
        html.push_str(&format!("<p>This is paragraph number {} with substantial real content about Rust dispatch mechanisms and how they work in practice with various types and traits and monomorphization.</p>", i));
    }
    html.push_str("</article></body></html>");
    assert!(
        html.len() > 20_000,
        "test HTML must be > 20KB, got {}",
        html.len()
    );
    let r = extract_html(&html);
    assert!(
        !r.thin,
        "large page with high content density must not be thin"
    );
}

/// A small page (< 20KB) with low content density is NOT thin.
/// Content density check only applies to large pages.
/// bilibili.com homepage: 24KB raw, ~1500 chars, 6% density.
#[test]
fn content_density_small_page_low_density_not_thin() {
    // 10KB of HTML with > 800 chars of text (density check doesn't fire,
    // and > 800 chars passes the existing thin check).
    let mut html = String::from("<html><head><script>");
    html.push_str(&"var x = 1;".repeat(500)); // ~4.5KB of JS
    html.push_str("</script></head><body>");
    html.push_str("<nav>Home About Contact Login Search Settings Downloads Community</nav>");
    html.push_str("<p>Some real content about video listings on the homepage of the site with enough text to exceed the 800 char threshold for thin detection and classification purposes.</p>");
    html.push_str("<p>More real content about the latest trending videos and live streams available on the platform right now for everyone to watch and enjoy at any time of day.</p>");
    html.push_str("<p>Additional content about categories including anime gaming music dance technology and more for users to explore at their leisure time on weekends.</p>");
    html.push_str("<p>Popular videos trending now live streams upcoming events new releases exclusive content creator highlights community picks recommended for you to watch.</p>");
    html.push_str("<p>Weekly top charts new creators rising stars featured playlists editor picks user favorites recently uploaded most discussed content on the platform today.</p>");
    html.push_str("<div>Breaking news updates latest announcements community events special promotions premium features subscriber benefits platform improvements new tools</div>");
    html.push_str("</body></html>");
    assert!(
        html.len() < 20_000,
        "test HTML must be < 20KB, got {}",
        html.len()
    );
    let r = extract_html(&html);
    assert!(
        !r.thin,
        "small page (< 20KB) must not trigger content density check, got thin={}, chars={}",
        r.thin, r.total_chars
    );
}

/// A large page with > 3000 chars extracted is NOT thin even if
/// density is low. The content is real even if the HTML is bloated.
#[test]
fn content_density_large_page_many_chars_not_thin() {
    // 80KB of HTML with ~3000 chars of text (7.5% density, but > 3000 chars).
    let mut html = String::from("<html><head><script>");
    html.push_str(&"var x = 1;".repeat(3000));
    html.push_str("</script></head><body><article>");
    html.push_str(
        &"<p>Real article content with enough text to exceed the 3000 char threshold.</p>"
            .repeat(40),
    );
    html.push_str("</article></body></html>");
    assert!(
        html.len() > 20_000,
        "test HTML must be > 20KB, got {}",
        html.len()
    );
    let r = extract_html(&html);
    assert!(
        !r.thin,
        "large page with > 3000 chars must not be thin even with low density"
    );
}

#[test]
fn short_pdf_is_not_classified_as_an_html_shell() {
    let doc = Html::parse_document("<html lang=\"en\"><title>Receipt</title></html>");
    let meta = metadata::metadata(&doc);
    let blocks = vec![blocks::Block::Para {
        md: "Total due: EUR 12".to_string(),
        link_density: 0.0,
        path: Vec::new(),
    }];
    let pages = vec![crate::pdf::PageMeta {
        page: 0,
        chars: 17,
        ocr: false,
        confidence: 1.0,
    }];
    let lang = language::detect(&doc);

    let out = downstream(
        &meta,
        blocks,
        25_000,
        false,
        false,
        Vec::new(),
        lang,
        Some(pages),
        "https://example.com/receipt.pdf",
        &ExtractOptions::default(),
        16_000,
        None,
    )
    .unwrap();

    assert!(!out.thin);
    assert!(!out.markdown.contains("JS-rendered"));
    assert_eq!(out.pdf_pages.as_ref().map(Vec::len), Some(1));
}

// ════════════════════════════════════════════════════════════
// 12. CROSS-BLOCK DEDUP
// ════════════════════════════════════════════════════════════

#[test]
fn dedup_identical_paragraphs() {
    // Paragraph-sized on purpose: a marker only stands in for a
    // block it is much shorter than, so a fixture trimmed to a few
    // words would be kept verbatim and test nothing.
    let html = r#"<html><body><article>
<p>Duplicate content that appears multiple times on this page, long enough that a marker standing in for it is the cheaper of the two.</p>
<p>Duplicate content that appears multiple times on this page, long enough that a marker standing in for it is the cheaper of the two.</p>
<p>Unique content that only appears once, and stays whatever the duplicate above it does.</p>
</article></body></html>"#;
    let r = extract_html(html);
    // "Duplicate content" should appear only once.
    let count = r.markdown.matches("Duplicate content").count();
    assert_eq!(count, 1);
    assert!(r.markdown.contains("Unique content"));
}

#[test]
fn dedup_badge_text() {
    let html = r#"<html><body><article>
<h1>Article</h1>
<p>5 min read</p>
<p>Real article content starts here with actual text.</p>
<p>5 min read</p>
</article></body></html>"#;
    let r = extract_html(html);
    // "5 min read" should appear at most once.
    let count = r.markdown.matches("5 min read").count();
    assert!(count <= 1);
}

// ════════════════════════════════════════════════════════════
// 12. JUNK FILTERING
// ════════════════════════════════════════════════════════════

#[test]
fn junk_nav_stripped() {
    let html = r#"<html><body>
<nav><a href="/">Home</a><a href="/blog">Blog</a><a href="/about">About</a></nav>
<article><p>Main article content that is long enough to be extracted as the primary content of this page.</p></article>
</body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("Main article content"));
    assert!(!r.markdown.contains("Home"));
    assert!(!r.markdown.contains("Blog"));
}

#[test]
fn junk_footer_stripped() {
    let html = r#"<html><body>
<article><p>Main article content that is long enough to be extracted as the primary content of this page.</p></article>
<footer><p>© 2026 Company. All rights reserved. Privacy Policy | Terms of Service</p></footer>
</body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("Main article content"));
    assert!(!r.markdown.contains("© 2026"));
    assert!(!r.markdown.contains("Privacy Policy"));
}

#[test]
fn junk_hidden_text() {
    let html = r#"<html><body><article>
<p>Visible content that should be extracted properly.</p>
<p style="display:none">Hidden content that should not appear.</p>
<p aria-hidden="true">ARIA hidden content.</p>
<p class="sr-only">Screen reader only text.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("Visible content"));
    assert!(!r.markdown.contains("Hidden content"));
    assert!(!r.markdown.contains("ARIA hidden"));
    assert!(!r.markdown.contains("Screen reader"));
}

#[test]
fn junk_script_style_stripped() {
    let html = r#"<html><body><article>
<p>Real content paragraph with sufficient text length.</p>
<script>var x = 1; console.log("junk");</script>
<style>.body { color: red; }</style>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("Real content"));
    assert!(!r.markdown.contains("console.log"));
    assert!(!r.markdown.contains("color: red"));
}

#[test]
fn junk_sidebar_size_gated() {
    // A "sidebar" class on a LARGE container should NOT be
    // stripped (false positive : could be the main content).
    let html = r#"<html><body>
<div class="sidebar">
<p>This is actually the main content despite the class name. It has substantial text content that makes it clearly the primary content of the page, not a sidebar at all. The size-gating should prevent this from being stripped by the junk filter even though the class name says sidebar.</p>
<p>More content in this sidebar container. It's long enough that the junk filter should recognize it as real content, not boilerplate. Adding more text here to ensure the total character count exceeds the four hundred character threshold that the size gating uses.</p>
<p>A third paragraph to further ensure the total text exceeds the size gate threshold and the content is properly extracted despite the misleading class name.</p>
</div>
</body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("actually the main content"));
}

// ════════════════════════════════════════════════════════════
// 13. AGENT-TRUST SIGNALS
// ════════════════════════════════════════════════════════════

#[test]
fn signal_focus_miss() {
    let html = r#"<html><body><article>
<p>The quick brown fox jumps over the lazy dog.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("nonexistent query term".to_string()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("*[focus"));
}

#[test]
fn signal_thin_js_shell() {
    // Large page with almost no extractable content → thin.
    let html = format!(
        r#"<html><body>
<div id="root" style="min-height:100vh"></div>
<script>{}</script>
</body></html>"#,
        "x".repeat(60_000)
    );
    let r = extract_html(&html);
    assert!(r.thin);
    assert!(r.markdown.contains("likely JS-rendered"));
}

#[test]
fn signal_not_thin_real_content() {
    let html = r#"<html><body><article>
<h1>Real Article</h1>
<p>This is a real article with substantial content that should not be flagged as thin. The content is meaningful and well-structured.</p>
<p>Another paragraph with additional content for the article.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(!r.thin);
}

// ════════════════════════════════════════════════════════════
// 14. QUALITY SCORE
// ════════════════════════════════════════════════════════════

#[test]
fn quality_score_well_structured() {
    let html = r#"<html lang="en"><head>
<title>Good Article</title>
<meta name="description" content="A well-structured article">
<meta name="author" content="Jane Doe">
<meta property="article:published_time" content="2026-01-01">
<meta property="og:site_name" content="Example">
</head><body><article>
<h1>Good Article</h1>
<p>A well-structured article with substantial content that is long enough to be meaningful and contribute to a high quality score.</p>
<h2>Section</h2>
<p>More substantial content in this section to ensure a good score.</p>
<ul><li>Point one</li><li>Point two</li></ul>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.quality > 0.5, "quality was {} expected > 0.5", r.quality);
}

#[test]
fn quality_score_poor_content() {
    let html = r#"<html><body></body></html>"#;
    let r = extract_html(html);
    assert!(r.quality < 0.3);
}

// ════════════════════════════════════════════════════════════
// 15. CHARSET DECODING
// ════════════════════════════════════════════════════════════

#[test]
fn charset_utf8_explicit() {
    let html =
        b"<html><head><meta charset='utf-8'></head><body><p>Caf\xc3\xa9 content</p></body></html>";
    let r = extract(
        html,
        "text/html; charset=utf-8",
        "https://example.com",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(r.markdown.contains("Café"));
}

#[test]
fn charset_bom_detection() {
    let html = b"\xEF\xBB\xBF<html><body><p>BOM-marked content here.</p></body></html>";
    let r = extract(
        html,
        "text/html",
        "https://example.com",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(r.markdown.contains("BOM-marked"));
}

#[test]
fn charset_meta_charset_tag() {
    // Latin-1 encoded "café" as 0xe9
    let html =
        b"<html><head><meta charset='iso-8859-1'></head><body><p>Caf\xe9 content</p></body></html>";
    let r = extract(
        html,
        "text/html",
        "https://example.com",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(r.markdown.contains("Café"));
}

const RU_FEED: &str = "<rss version=\"2.0\"><channel><title>Новости</title><link>https://news.example/</link><item><title>Первая новость дня</title><link>https://news.example/1</link><description>Текст.</description></item></channel></rss>";

fn feed_markdown(body: &[u8], content_type: &str) -> String {
    extract(
        body,
        content_type,
        "https://news.example/feed",
        &ExtractOptions::default(),
    )
    .unwrap()
    .markdown
}

#[test]
fn plain_text_honours_its_declared_charset() {
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode("Привет, мир! Обычный текстовый файл.\n");
    let ex = extract(
        &cp1251,
        "text/plain; charset=windows-1251",
        "https://example.com/a.txt",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(ex.markdown.contains("Привет, мир!"), "{:?}", ex.markdown);
    let (latin1, _, _) = encoding_rs::WINDOWS_1252.encode("Café crème, Größe 12 °C, £5.\n");
    let ex = extract(
        &latin1,
        "text/plain; charset=iso-8859-1",
        "https://example.com/b.txt",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(ex.markdown.contains("Café crème"), "{:?}", ex.markdown);
}

#[test]
fn feed_honours_its_xml_encoding() {
    let feed = format!("<?xml version=\"1.0\" encoding=\"windows-1251\"?>{RU_FEED}");
    let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(&feed);
    let md = feed_markdown(&bytes, "application/rss+xml");
    assert!(md.contains("Первая новость дня"), "{md:?}");
}

#[test]
fn feed_honours_a_header_charset() {
    let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(RU_FEED);
    let md = feed_markdown(&bytes, "application/rss+xml; charset=windows-1251");
    assert!(md.contains("Первая новость дня"), "{md:?}");
}

// RFC 7303 §3.2: without a BOM, the charset parameter is authoritative
// over the XML declaration.
#[test]
fn a_header_charset_outranks_the_xml_declaration() {
    for (header, declared) in [("windows-1251", "iso-8859-1"), ("utf-8", "windows-1251")] {
        let feed = format!("<?xml version=\"1.0\" encoding=\"{declared}\"?>{RU_FEED}");
        let bytes = encoding_rs::Encoding::for_label(header.as_bytes())
            .unwrap()
            .encode(&feed)
            .0;
        let md = feed_markdown(&bytes, &format!("application/rss+xml; charset={header}"));
        assert!(md.contains("Первая новость дня"), "{header}: {md:?}");
    }
}

#[test]
fn undeclared_utf8_text_and_feeds_still_decode_as_utf8() {
    // The word "charset" in a text body is not a declaration.
    let text = "Größe 12 °C, café.\nThe old copy was served with charset=iso-8859-1.\n";
    let ex = extract(
        text.as_bytes(),
        "text/plain",
        "https://example.com/c.txt",
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(ex.markdown, text);
    for (prolog, content_type) in [
        ("<?xml version=\"1.0\"?>", "application/rss+xml"),
        (
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "text/xml; charset=utf-8",
        ),
    ] {
        let md = feed_markdown(format!("{prolog}{RU_FEED}").as_bytes(), content_type);
        assert!(md.contains("# Новости"), "{md:?}");
        assert!(md.contains("Первая новость дня"), "{md:?}");
        assert!(!md.contains('\u{FFFD}'), "{md:?}");
    }
}

// ════════════════════════════════════════════════════════════
// 16. CSS SELECTOR
// ════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════
// 17. MIXED LANGUAGE PAGES
// ════════════════════════════════════════════════════════════

#[test]
fn mixed_cjk_latin_tech_page() {
    let html = r#"<html lang="zh"><body><article>
<h1>Python编程入门</h1>
<p>Python是一种流行的编程语言，广泛用于数据科学和机器学习。</p>
<h2>安装 Python</h2>
<p>使用 pip install 命令安装依赖包。</p>
<pre><code class="language-python">print("Hello, World!")</code></pre>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.lang, "zh");
    assert!(r.markdown.contains("Python编程入门"));
    assert!(r.markdown.contains("数据科学"));
    assert!(r.markdown.contains("```python"));
    assert!(r.markdown.contains("Hello, World!"));
}

#[test]
fn mixed_language_focus_cjk() {
    let html = r#"<html lang="zh"><body><article>
<h1>Web开发技术</h1>
<p>React是Facebook开发的JavaScript库，用于构建用户界面。</p>
<p>Django是Python的Web框架，适合快速开发。</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("React".to_string()),
            ..Default::default()
        },
    );
    // "React" is Latin mixed into CJK : should still match.
    assert!(!r.markdown.contains("no matches"), "{}", r.markdown);
    assert!(r.markdown.contains("React") || r.markdown.contains("Facebook"));
}

// ════════════════════════════════════════════════════════════
// 18. CONTENT KIND: TABLE-HEAVY
// ════════════════════════════════════════════════════════════

#[test]
fn content_kind_table() {
    let html = r#"<html><body><article>
<h1>Comparison Table</h1>
<table>
<tr><th>Feature</th><th>Rust</th><th>Go</th></tr>
<tr><td>Memory</td><td>Safe</td><td>GC</td></tr>
<tr><td>Speed</td><td>Fast</td><td>Fast</td></tr>
<tr><td>Safety</td><td>Yes</td><td>Partial</td></tr>
</table>
<table>
<tr><th>Tool</th><th>Type</th></tr>
<tr><td>cargo</td><td>Build</td></tr>
<tr><td>go</td><td>Build</td></tr>
</table>
</article></body></html>"#;
    let r = extract_html(html);
    assert_eq!(r.content_kind, ContentKind::Table);
}

// ════════════════════════════════════════════════════════════
// 19. LINK FARM SUPPRESSION
// ════════════════════════════════════════════════════════════

#[test]
fn link_farm_list_suppressed() {
    let html = r#"<html><body><article>
<ul>
<li><a href="/page1">Link one</a></li>
<li><a href="/page2">Link two</a></li>
<li><a href="/page3">Link three</a></li>
<li><a href="/page4">Link four</a></li>
<li><a href="/page5">Link five</a></li>
<li><a href="/page6">Link six</a></li>
<li><a href="/page7">Link seven</a></li>
</ul>
<p>Real content paragraph with actual text.</p>
</article></body></html>"#;
    let r = extract_html(html);
    // Link farm list (>6 items, all links) should be dropped.
    assert!(!r.markdown.contains("Link one"));
    assert!(!r.markdown.contains("Link seven"));
    assert!(r.markdown.contains("Real content"));
}

// ════════════════════════════════════════════════════════════
// 20. TITLE/HEADING DEDUP
// ════════════════════════════════════════════════════════════

#[test]
fn title_h1_dedup() {
    let html = r#"<html><head><title>Unique Title</title></head>
<body><article>
<h1>Unique Title</h1>
<p>Content paragraph with enough text to be extracted properly.</p>
</article></body></html>"#;
    let r = extract_html(html);
    // "Unique Title" should appear at most once (no duplication).
    let count = r.markdown.matches("Unique Title").count();
    assert!(count <= 1, "title appeared {} times, expected <= 1", count);
}

// ════════════════════════════════════════════════════════════
// 21. BARE-LINK / NOISE SUPPRESSION
// ════════════════════════════════════════════════════════════

#[test]
fn bare_link_lines_suppressed() {
    let html = r#"<html><body><article>
<p>Real content paragraph here.</p>
<p><a href="https://example.com">barelinktext</a></p>
<p>More real content.</p>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("Real content"));
    // Bare-link line (short, all-link) should be dropped.
    assert!(!r.markdown.contains("barelinktext"));
}

// ════════════════════════════════════════════════════════════
// 22. FRENCH-ACCENT FOCUS + STEMMING
// ════════════════════════════════════════════════════════════

#[test]
fn focus_french_stemming() {
    let html = r#"<html lang="fr"><body><article>
<p>Les ordinateurs apprennent à partir des données.</p>
<p>Le temps est beau aujourd'hui.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("ordinateur".to_string()),
            ..Default::default()
        },
    );
    // "ordinateurs" → stem "ordinateur" should match query.
    assert!(!r.markdown.contains("no matches"), "{}", r.markdown);
}

// ════════════════════════════════════════════════════════════
// 23. JSON-LD UNICODE ESCAPE DECODING
// ════════════════════════════════════════════════════════════

#[test]
fn jsonld_unicode_escape_decoded() {
    let html = r#"<html><head>
<script type="application/ld+json">
{"@type":"Article","author":{"name":"\u7ef4\u57fa\u5a92\u4f53"},"datePublished":"2026-02-01"}
</script>
</head><body><p>Content</p></body></html>"#;
    let doc = Html::parse_document(html);
    let meta = metadata::metadata(&doc);
    // Should decode to actual Chinese characters, not escape sequences.
    assert_eq!(meta.byline.as_deref(), Some("维基媒体"));
}

// html5ever's tree builder scans the open-element stack per tag: the
// parse is O(n²) in nesting depth (8 000 nested <div> 0.4 s, 32 000
// 6.6 s measured here) and nothing capped the depth (browsers stop at
// 512). The gate refuses such a body before the parse; a wide page of
// the same size is untouched.
#[test]
fn a_page_nested_thousands_deep_is_refused_before_the_parse() {
    let n = 50_000;
    let html = format!(
        "<html><body><article>{}x{}</article></body></html>",
        "<div>".repeat(n),
        "</div>".repeat(n)
    );
    let started = std::time::Instant::now();
    let err = extract(
        html.as_bytes(),
        "text/html",
        "https://ex.com/",
        &ExtractOptions::default(),
    )
    .err()
    .expect("refused");
    assert!(err.to_string().contains("nested deeper"), "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    // The self-closing slash is ignored by the HTML parser, so this
    // shape nests for real too and is refused the same way.
    let html = format!(
        "<html><body><article>{}x</article></body></html>",
        "<div/>".repeat(n)
    );
    let err = extract(
        html.as_bytes(),
        "text/html",
        "https://ex.com/",
        &ExtractOptions::default(),
    )
    .err()
    .expect("refused");
    assert!(err.to_string().contains("nested deeper"), "{err}");
    // Same byte count, wide instead of deep: extracted as usual.
    let html = format!(
        "<html><body><article>{}</article></body></html>",
        "<p>a paragraph with enough words to be kept by the extractor here</p>".repeat(n / 10)
    );
    let r = extract_html(&html);
    assert!(r.markdown.contains("a paragraph"));
}

// ════════════════════════════════════════════════════════════
// 24. TABLE WITHOUT <th> : FIRST ROW PROMOTED TO HEADERS
// ════════════════════════════════════════════════════════════

// Tables nested d deep: the table scans used descendant selects, so
// every level re-scanned every nested cell below it, quadratic per
// level and the walk recursed a level and repeated it. 1000 levels
// (18 KB) took minutes on the tokio worker.
#[test]
fn deeply_nested_tables_extract_in_linear_time() {
    let d = 1000;
    let html = format!(
        "<html><body><article>{}x{}</article></body></html>",
        "<table><tr><td>".repeat(d),
        "</td></tr></table>".repeat(d)
    );
    let started = std::time::Instant::now();
    let r = extract_html(&html);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    assert!(r.markdown.contains('x'));
}

// A table nested in a cell is that cell's content, not extra rows of
// the outer table.
#[test]
fn nested_table_rows_are_not_merged_into_the_outer_table() {
    let html = r#"<html><body><article>
<table>
<tr><th>Key</th><th>Value</th></tr>
<tr><td>inner</td><td><table><tr><td>a</td></tr><tr><td>b</td></tr><tr><td>c</td></tr></table></td></tr>
<tr><td>after</td><td>done</td></tr>
</table>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(r.markdown.contains("| Key | Value |"), "{}", r.markdown);
    assert!(r.markdown.contains("| after | done |"), "{}", r.markdown);
    // The inner rows do not become `| a |` rows of the outer table.
    assert!(!r.markdown.contains("| a |"), "{}", r.markdown);
}

#[test]
fn table_without_th_promotes_first_row() {
    let html = r#"<html><body><article>
<table>
<tr><td>Name</td><td>Value</td><td>Description</td></tr>
<tr><td>Rust</td><td>1.75</td><td>Systems language</td></tr>
<tr><td>Go</td><td>1.22</td><td>Simple concurrency</td></tr>
</table>
</article></body></html>"#;
    let r = extract_html(html);
    // First row should be headers, not empty | | |.
    assert!(r.markdown.contains("| Name | Value |"));
    assert!(r.markdown.contains("| Rust | 1.75 |"));
    // Should NOT have empty header row.
    assert!(!r.markdown.contains("|  |  |"));
}

// A table of one row is a figure ("Maximum | EUR 15M or 3%"), not
// navigation: table_block dropped every table of three cells or fewer
// and under 60 chars, so a section read as complete while its one
// number was missing. Only a tiny table made of links is tab-nav.
#[test]
fn a_small_table_of_figures_is_kept_and_a_tiny_link_table_is_not() {
    let prose = "The ceiling for this tier is stated in the table that follows, and small enterprises pay the lower of the two amounts rather than the higher one.";
    let html = format!(
        r#"<html><body><article><h1>Fines</h1>
<h2>Tier two</h2><p>{prose}</p>
<table><tr><th>Maximum</th><td>EUR 15M or 3%</td></tr></table>
<p>SMEs pay the lower amount.</p>
<h2>Tier one</h2><p>{prose}</p>
<table><tr><td>Max</td><td>EUR 35M</td><td>7%</td></tr></table>
<h2>Tabs</h2><p>{prose}</p>
<table><tr><td><a href="/a">Overview</a></td><td><a href="/b">Pricing</a></td></tr></table>
</article></body></html>"#
    );
    let r = extract_html(&html);
    assert!(r.markdown.contains("EUR 15M or 3%"), "{}", r.markdown);
    assert!(r.markdown.contains("EUR 35M"), "{}", r.markdown);
    assert!(!r.markdown.contains("Overview"), "{}", r.markdown);

    // And the section view carries the figure with its prose.
    let view = view::View {
        section: Some("Tier two".into()),
        max_chars: 16_000,
        ..view::View::default()
    };
    let section = view::render(&r.markdown, &view, None);
    assert!(section.text.contains("EUR 15M or 3%"), "{}", section.text);
    assert!(section.text.contains("lower amount"), "{}", section.text);
}

// A series sidebar is a table of links to other articles. It is not
// content of the page, and at thousands of characters it led every read of
// the article; a data table with the same class is still data.
#[test]
fn a_link_directory_table_is_dropped_but_a_data_table_is_not() {
    let prose = "The treaty was signed after long negotiation between the parties, and it entered into force a year later once every signatory had ratified it in turn.";
    let links: String = (0..40)
        .map(|i| format!(r#"<tr><td><a href="/wiki/Topic_{i}">Topic number {i}</a></td></tr>"#))
        .collect();
    let html = format!(
        r#"<html><body><article><h1>Treaty</h1><p>{prose}</p>
<table class="sidebar nomobile">{links}</table>
<h2>Terms</h2><p>{prose}</p>
<table class="sidebar"><tr><th>Item</th><th>Limit</th></tr><tr><td>Fines</td><td>EUR 15M or 3%</td></tr></table>
</article></body></html>"#
    );
    let r = extract_html(&html);
    assert!(!r.markdown.contains("Topic number 7"), "{}", r.markdown);
    assert!(r.markdown.contains("EUR 15M or 3%"), "{}", r.markdown);
    assert!(r.markdown.contains("long negotiation"), "{}", r.markdown);
}

// ════════════════════════════════════════════════════════════
// 24b. TABLE ROW HEADERS (<th scope="row">) ARE KEPT
// ════════════════════════════════════════════════════════════

// The standard accessible markup for a data table labels each row
// with a <th scope="row">. table_block collected <th> and <td> with
// two separate selects and only ever used <th> cells for the single
// header row, so every later row silently lost its label and its
// remaining cells shifted one column left.
#[test]
fn table_row_header_cells_are_kept_in_data_rows() {
    let html = r#"<html><body><article>
<table>
<tr><th></th><th>2022</th><th>2023</th></tr>
<tr><th scope="row">Revenue</th><td>10</td><td>20</td></tr>
<tr><th scope="row">Costs</th><td>7</td><td>9</td></tr>
</table>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(
        r.markdown.contains("| Revenue | 10 | 20 |"),
        "row label dropped:\n{}",
        r.markdown
    );
    assert!(r.markdown.contains("| Costs | 7 | 9 |"));
}

// A first row that mixes <th> and <td> is a data row, not a header
// row; its <td> cells must not be thrown away.
#[test]
fn table_mixed_first_row_keeps_td_cells() {
    let html = r#"<html><body><article>
<table>
<tr><th>Language</th><td>Rust</td></tr>
<tr><th>License</th><td>MIT</td></tr>
<tr><th>Since</th><td>2015</td></tr>
</table>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(
        r.markdown.contains("Rust") && r.markdown.contains("MIT") && r.markdown.contains("2015"),
        "td cells dropped:\n{}",
        r.markdown
    );
    assert!(
        r.markdown.contains("| License | MIT |"),
        "label/value pairing lost:\n{}",
        r.markdown
    );
}

// ════════════════════════════════════════════════════════════
// 24c. ROWSPAN / COLSPAN : CELLS STAY IN THEIR GRID COLUMN
// ════════════════════════════════════════════════════════════

fn table_page(tables: &str) -> String {
    format!("<html><body><article>{tables}</article></body></html>")
}

fn table_lines(md: &str) -> Vec<&str> {
    md.lines().filter(|l| l.starts_with('|')).collect()
}

// A cell with rowspan="2" also fills its column in the next row. Read
// as a flat cell list, that row's cells moved one column left: Banana
// under Category, 120 g under Item, Weight empty.
#[test]
fn a_rowspan_keeps_the_next_rows_cells_under_their_headers() {
    let md = extract_html(&table_page(
        "<table><tr><th>Category</th><th>Item</th><th>Weight</th></tr>\
         <tr><td rowspan=\"2\">Fruit</td><td>Apple</td><td>150 g</td></tr>\
         <tr><td>Banana</td><td>120 g</td></tr></table>",
    ))
    .markdown;
    assert_eq!(
        table_lines(&md),
        [
            "| Category | Item | Weight |",
            "| --- | --- | --- |",
            "| Fruit | Apple | 150 g |",
            "| Fruit | Banana | 120 g |",
        ],
        "{md}"
    );
}

// A grouped header: "Price" spans two columns and its sub-headers sit
// in a second <th> row. The first row had two cells for three columns
// and the second became a data row; both rows form one header now.
#[test]
fn a_colspan_header_and_its_sub_headers_form_one_header_row() {
    let md = extract_html(&table_page(
        "<table><tr><th rowspan=\"2\">Model</th><th colspan=\"2\">Price</th></tr>\
         <tr><th>Monthly</th><th>Yearly</th></tr>\
         <tr><td>Basic</td><td>$5</td><td>$50</td></tr>\
         <tr><td>Free</td><td colspan=\"2\">n/a</td></tr></table>",
    ))
    .markdown;
    assert_eq!(
        table_lines(&md),
        [
            "| Model | Price Monthly | Price Yearly |",
            "| --- | --- | --- |",
            "| Basic | $5 | $50 |",
            "| Free | n/a | n/a |",
        ],
        "{md}"
    );
}

// rowspan="0" spans the rest of its row group, and a rowspan larger
// than the group stops at the group's last row instead of reaching
// into the next tbody.
#[test]
fn a_rowspan_ends_with_its_row_group() {
    let md = extract_html(&table_page(
        "<table><thead><tr><th>Region</th><th>City</th></tr></thead>\
         <tbody><tr><td rowspan=\"0\">North</td><td>Oslo</td></tr><tr><td>Bergen</td></tr></tbody>\
         <tbody><tr><td rowspan=\"5\">South</td><td>Rome</td></tr><tr><td>Naples</td></tr></tbody>\
         <tbody><tr><td>East</td><td>Delhi</td></tr></tbody></table>",
    ))
    .markdown;
    assert_eq!(
        table_lines(&md),
        [
            "| Region | City |",
            "| --- | --- |",
            "| North | Oslo |",
            "| North | Bergen |",
            "| South | Rome |",
            "| South | Naples |",
            "| East | Delhi |",
        ],
        "{md}"
    );
}

// Focus keeps only the rows it selects, so a selected row has to carry
// its rowspan group label itself.
#[test]
fn a_focused_table_row_keeps_its_rowspan_label() {
    let html = table_page(
        "<h1>Pantry</h1><p>Weights of the pantry stock, measured per item.</p>\
         <table><tr><th>Group</th><th>Item</th><th>Weight</th></tr>\
         <tr><td rowspan=\"4\">Fruit</td><td>Apple</td><td>150 g</td></tr>\
         <tr><td>Banana</td><td>120 g</td></tr><tr><td>Cherry</td><td>8 g</td></tr>\
         <tr><td>Grape</td><td>5 g</td></tr>\
         <tr><td rowspan=\"4\">Root</td><td>Carrot</td><td>60 g</td></tr>\
         <tr><td>Beet</td><td>90 g</td></tr><tr><td>Radish</td><td>20 g</td></tr>\
         <tr><td>Turnip</td><td>110 g</td></tr></table>",
    );
    let md = extract_html_opts(
        &html,
        &ExtractOptions {
            focus: Some("banana".into()),
            ..Default::default()
        },
    )
    .markdown;
    assert!(md.contains("| Fruit | Banana | 120 g |"), "{md}");
    assert!(!md.contains("Carrot"), "focus kept every row:\n{md}");
}

// Negative: without a span the table renders exactly as before. The
// second all-<th> row stays a data row (no header cell above it spans
// columns), the ragged row keeps its extra cell, the empty row is
// dropped, and span attributes that parse to 1 or fail to parse change
// nothing.
#[test]
fn a_table_without_spans_renders_as_before() {
    let md = extract_html(&table_page(
        "<table><tr><th>Name</th><th colspan=\"1\">Score</th></tr>\
         <tr><th>Unit</th><th>pts</th></tr>\
         <tr><th scope=\"row\">Ada</th><td rowspan=\"x\">9</td><td colspan=\"-2\">extra</td></tr>\
         <tr><td></td><td></td></tr>\
         <tr><td rowspan=\"1\">Bob</td></tr></table>",
    ))
    .markdown;
    assert_eq!(
        table_lines(&md),
        [
            "| Name | Score |  |",
            "| --- | --- | --- |",
            "| Unit | pts |  |",
            "| Ada | 9 | extra |",
            "| Bob |  |  |",
        ],
        "{md}"
    );
}

// Negative: a cell over the table's whole width (an infobox title or
// section row) is written once, in the first column, as before.
#[test]
fn a_full_width_cell_is_written_once() {
    let md = extract_html(&table_page(
        "<table><tr><th colspan=\"2\">Rust</th></tr>\
         <tr><th>Paradigms</th><td>Multi-paradigm</td></tr>\
         <tr><td colspan=\"2\">Influenced by</td></tr>\
         <tr><td colspan=\"2\">Alef BETA CLU</td></tr></table>",
    ))
    .markdown;
    assert_eq!(
        table_lines(&md),
        [
            "| Rust |  |",
            "| --- | --- |",
            "| Paradigms | Multi-paradigm |",
            "| Influenced by |  |",
            "| Alef BETA CLU |  |",
        ],
        "{md}"
    );
}

// Negative: a layout table (prose cells, no <th>) is still walked as
// paragraphs, and a three-link strip still falls to the tab-nav filter
// when one link spans two columns: that filter counts source cells, not
// the slots a span fills.
#[test]
fn layout_and_link_strip_tables_are_not_turned_into_grids() {
    let prose = "This cell holds a paragraph of running text, the way forum and email layouts put content in tables. ".repeat(4);
    let md = extract_html(&table_page(&format!(
        "<p>Lead paragraph of the article, long enough to stand as its body text here.</p>\
         <table><tr><td><p>{prose}</p></td><td><p>Second column text.</p></td></tr></table>\
         <table><tr><td colspan=\"2\"><a href=\"/home\">Home</a></td></tr>\
         <tr><td><a href=\"/docs\">Docs</a></td><td><a href=\"/blog\">Blog</a></td></tr></table>"
    )))
    .markdown;
    assert!(table_lines(&md).is_empty(), "{md}");
    assert!(md.contains("Second column text."), "{md}");
    assert!(!md.contains("Docs"), "{md}");
}

// Negative: hostile span values stay bounded. colspan=100000 and
// rowspan=100000 on every row, and a long cell whose rowspan reaches
// thousands of rows: the grid stays a few columns wide and the copies
// of the long cell stop at the copy budget.
#[test]
fn hostile_span_values_stay_bounded() {
    let long = "w".repeat(4000);
    let mut t = format!("<table><tr><th colspan=\"100000\">{long}</th></tr>");
    for i in 0..3000 {
        t.push_str(&format!(
            "<tr><td colspan=\"100000\" rowspan=\"100000\">c{i}</td><td rowspan=\"0\">d{i}</td></tr>"
        ));
    }
    // The header row keeps this table a data table despite its long cell.
    t.push_str("</table><table><tr><th>Note</th><th>Value</th></tr>");
    t.push_str(&format!(
        "<tr><td rowspan=\"100000\" colspan=\"100000\">{long}</td><td>v0</td></tr>"
    ));
    for i in 1..3000 {
        t.push_str(&format!("<tr><td>v{i}</td></tr>"));
    }
    t.push_str("</table>");
    let started = std::time::Instant::now();
    let md = extract_html(&table_page(&t)).markdown;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    let lines = table_lines(&md);
    assert!(lines.len() <= 2 * (3000 + 2), "{} table lines", lines.len());
    let widest = lines.iter().map(|l| l.matches(" | ").count() + 1).max();
    assert!(widest.unwrap_or(0) <= 8, "{widest:?} columns");
    assert!(md.len() < 512 << 10, "{} bytes of markdown", md.len());
}

// Negative: a few cells that span many rows cannot build a grid far
// larger than the table's source. 2000 header cells with rowspan="0"
// over 2000 one-cell rows would be 2000 x 2001 slots; past
// max(65536, 8 x source cells) slots the spans end.
#[test]
fn a_rowspan_grid_stays_proportional_to_its_source_cells() {
    let mut t = String::from("<table><tr>");
    for i in 0..2000 {
        t.push_str(&format!("<th rowspan=\"0\">k{i}</th>"));
    }
    t.push_str("</tr>");
    for i in 0..2000 {
        t.push_str(&format!("<tr><td>v{i}</td></tr>"));
    }
    t.push_str("</table>");
    let doc = Html::parse_document(&table_page(&t));
    let mut out = Vec::new();
    blocks::segment(
        doc.root_element(),
        "https://example.com/",
        &ExtractOptions::default(),
        &mut out,
    );
    let slots: usize = out
        .iter()
        .map(|b| match b {
            blocks::Block::Table { headers, rows, .. } => {
                headers.len() + rows.iter().map(Vec::len).sum::<usize>()
            }
            _ => 0,
        })
        .sum();
    assert!(slots >= 4000, "{slots} slots");
    assert!(slots <= (1 << 16) + 2 * 2001, "{slots} slots");
}

// ════════════════════════════════════════════════════════════
// 25. TABLE CELL TRUNCATION : CHAR-BASED (CJK)
// ════════════════════════════════════════════════════════════

#[test]
fn table_cjk_cell_not_over_truncated() {
    let long_cjk: String = "测试".repeat(80);
    let html = format!(
        r#"<html><body><article>
<table>
<tr><th>Column</th></tr>
<tr><td>{long_cjk}</td></tr>
</table>
</article></body></html>"#
    );
    let r = extract_html(&html);
    let cjk_count = r
        .markdown
        .chars()
        .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
        .count();
    assert!(
        cjk_count >= 100,
        "CJK chars in table: {cjk_count}, expected >= 100"
    );
}

// ════════════════════════════════════════════════════════════
// 26. CODE BLOCK BLANK LINE COLLAPSE
// ════════════════════════════════════════════════════════════

#[test]
fn code_block_blank_lines_collapsed() {
    let html = r#"<html><body><article>
<pre><code class="language-rust">fn main() {
    println!("Hello");



    println!("World");
}</code></pre>
</article></body></html>"#;
    let r = extract_html(html);
    let max_consecutive = r
        .markdown
        .lines()
        .fold((0, 0), |(max, current), line| {
            if line.trim().is_empty() {
                (max.max(current + 1), current + 1)
            } else {
                (max, 0)
            }
        })
        .0;
    assert!(
        max_consecutive <= 1,
        "Found {max_consecutive} consecutive blank lines, expected <= 1"
    );
}

// ════════════════════════════════════════════════════════════
// 27. <details>/<summary> HANDLING
// ════════════════════════════════════════════════════════════

#[test]
fn details_summary_as_heading() {
    let html = r#"<html><body><article>
<h1>API Reference</h1>
<p>Introduction to the API.</p>
<details>
<summary>Advanced Configuration</summary>
<p>Set the timeout to 30 seconds for production use.</p>
</details>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(
        r.markdown.contains("### Advanced Configuration"),
        "Expected ### Advanced Configuration, got: {}",
        r.markdown
    );
    assert!(r.markdown.contains("timeout to 30 seconds"));
}

// ════════════════════════════════════════════════════════════
// 28. CODE BLOCK LANGUAGE DETECTION : MULTIPLE PATTERNS
// ════════════════════════════════════════════════════════════

#[test]
fn code_lang_detection_patterns() {
    let html1 = r#"<html><body><article>
<pre><code class="language-python">print(1)</code></pre>
</article></body></html>"#;
    assert!(extract_html(html1).markdown.contains("```python"));

    let html2 = r#"<html><body><article>
<pre><code class="lang-go">fmt.Println(1)</code></pre>
</article></body></html>"#;
    assert!(extract_html(html2).markdown.contains("```go"));

    let html3 = r#"<html><body><article>
<pre><code class="javascript">console.log(1)</code></pre>
</article></body></html>"#;
    assert!(extract_html(html3).markdown.contains("```javascript"));
}

// MDN puts the language on the <pre> (SyntaxHighlighter's `brush:`) and
// repeats it as a visible header label. The fence must carry it, and the
// label must not survive as a stray one-word paragraph (it did: a bare "js"
// line above every example).
#[test]
fn a_code_label_on_the_pre_element_reaches_the_fence() {
    let html = r#"<html><body><article><h2>Example</h2>
<div class="code-example">
<p class="example-header"><span class="language-name">js</span></p>
<pre class="brush: js notranslate"><code>const r = await fetch(u);</code></pre>
</div>
</article></body></html>"#;
    let md = extract_html(html).markdown;
    assert!(md.contains("```js\n"), "fence lost its language:\n{md}");
    assert!(
        !md.lines().any(|l| l.trim() == "js"),
        "the label survived as a paragraph:\n{md}"
    );
}

#[test]
fn a_lone_language_word_above_a_fence_becomes_its_info_string() {
    let html = r#"<html><body><article><h2>Install</h2>
<p>Run this:</p>
<p>Bash</p>
<pre><code>cargo install donsetch</code></pre>
</article></body></html>"#;
    let md = extract_html(html).markdown;
    assert!(md.contains("```bash\n"), "label not folded:\n{md}");
    assert_eq!(md.matches("Bash").count(), 0, "label printed:\n{md}");
}

#[test]
fn a_code_block_that_already_has_a_language_keeps_its_paragraph() {
    // A one-word paragraph before an already-tagged fence is prose, not a
    // label: nothing may be dropped.
    let html = r#"<html><body><article><h2>Notes</h2>
<p><strong>Rust</strong> is the language used below.</p>
<pre><code class="language-rust">fn main() {}</code></pre>
</article></body></html>"#;
    let md = extract_html(html).markdown;
    assert!(md.contains("is the language used below"), "{md}");
    assert!(md.contains("```rust"));
}

// ════════════════════════════════════════════════════════════
// 29. PAGINATE START : RESUMES AT BLOCK BOUNDARY
// ════════════════════════════════════════════════════════════

#[test]
fn paginate_resume_starts_at_block_boundary() {
    let mut html = String::from("<html><body><article><h1>Page</h1>");
    for i in 0..50 {
        html.push_str(&format!(
            "<p>Paragraph {i}: Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod.</p>"
        ));
    }
    html.push_str("</article></body></html>");
    let r1 = extract_html_opts(
        &html,
        &ExtractOptions {
            max_chars: Some(800),
            ..Default::default()
        },
    );
    let offset = r1.next_offset.expect("should have next offset");
    let r2 = extract_html_opts(
        &html,
        &ExtractOptions {
            max_chars: Some(800),
            offset,
            ..Default::default()
        },
    );
    let first_content = r2
        .markdown
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with("https://") && !l.starts_with("*["))
        .unwrap_or("");
    assert!(
        first_content.is_empty()
            || first_content.starts_with('#')
            || first_content.starts_with("Paragraph")
            || first_content.starts_with("Lorem")
            || first_content
                .chars()
                .next()
                .is_some_and(|c| c.is_uppercase()),
        "Page 2 starts with: '{first_content}'"
    );
}

// ════════════════════════════════════════════════════════════
// 30. DESCRIPTION CAP : 500 CHARS MAX
// ════════════════════════════════════════════════════════════

#[test]
fn description_capped_at_500() {
    let long_desc = "A".repeat(1000);
    let html = format!(
        r#"<html><head>
<meta name="description" content="{long_desc}">
</head><body><article>
<h1>Test</h1>
<p>Content paragraph with sufficient text for extraction.</p>
</article></body></html>"#
    );
    let r = extract_html(&html);
    let desc_line = r
        .markdown
        .lines()
        .find(|l| l.starts_with("> "))
        .unwrap_or("");
    let desc = desc_line.strip_prefix("> ").unwrap_or("");
    assert!(
        desc.len() <= 500,
        "Description is {} chars, expected <= 500",
        desc.len()
    );
}

#[test]
fn wiki_math_inline_recovered_live_shape() {
    // Exact live shape from en.wikipedia.org: p > span.mwe-math-element
    // > (visible img fallback) + HIDDEN span (display:none) > math[alttext].
    // The hidden-math exception in junk::skip must let it through.
    let alttext = r"{\displaystyle A=\mathrm{softmax}\left(\frac{QK^{T}}{\sqrt{d_{k}}}\right)V}";
    let html = format!(
        r#"<html><body><div class="mw-parser-output"><p>The scaled dot-product attention is defined as: <span class="mwe-math-element"><span class="mwe-math-fallback-image-inline"><img src="x.svg" alt="formula"></span><span class="mwe-math-mathml-inline mwe-math-mathml-a11y" style="display: none;"><math xmlns="http://www.w3.org/1998/Math/MathML" alttext="{alt}"><semantics><mrow><mi>A</mi></mrow><annotation encoding="application/x-tex">A</annotation></semantics></math></span></span> where the softmax function is applied.</p></div></body></html>"#,
        alt = alttext.replace('"', "&quot;")
    );
    let ex = crate::extract::extract(
        html.as_bytes(),
        "text/html",
        "https://en.wikipedia.org/wiki/X",
        &crate::extract::ExtractOptions::default(),
    )
    .unwrap();
    assert!(
        ex.markdown.contains("$"),
        "no inline math rendered: {}",
        ex.markdown
    );
    assert!(ex.markdown.contains("softmax"), "{}", ex.markdown);
    assert!(ex.markdown.contains("d_{k}"), "{}", ex.markdown);
}

#[test]
fn hn_extractor_fires_on_live_thread_shape() {
    if let Ok(html) = std::fs::read_to_string("/tmp/hn_thread.html") {
        let ex = crate::extract::extract(
            html.as_bytes(),
            "text/html",
            "https://news.ycombinator.com/item?id=41975047",
            &crate::extract::ExtractOptions::default(),
        )
        .unwrap();
        if !ex.markdown.contains("## Discussion") {
            panic!(
                "PIPELINE MISMATCH. first 300: {}\n\ntotal={} blocks={} kind={:?}",
                &ex.markdown[..300.min(ex.markdown.len())],
                ex.total_chars,
                ex.blocks_total,
                ex.content_kind
            );
        }
    }
}

// ════════════════════════════════════════════════════════════
// 30. PAGINATE : HOSTILE ARGS MUST NOT PANIC
// ════════════════════════════════════════════════════════════

#[test]
fn paginate_huge_offset_and_max_no_panic() {
    // offset + max_chars == usize::MAX used to wrap end below start
    // and panic on the slice. Saturating arithmetic must return the
    // tail (offset near EOF → empty) without panicking.
    let text = "段落一\n\n段落二\n\n段落三".repeat(50);
    let (slice, next) = crate::extract::paginate_public(&text, 1, usize::MAX - 1);
    assert!(!slice.is_empty());
    assert_eq!(next, None);
    let (empty, next2) = crate::extract::paginate_public(&text, text.len() + 10, 1000);
    assert_eq!(empty, "");
    assert_eq!(next2, None);
}

// ── v3: dropped-content manifest ─────────────────────────────

#[test]
fn v3_dropped_manifest_present_when_focus_drops_blocks() {
    let html = r#"<html><body><article>
<h1>Guide</h1>
<p>The rust ownership model moves values by default.</p>
<p>Borrowing prevents aliasing and mutation at once.</p>
<h2>Related posts</h2>
<p>Check out our other articles about cooking and travel.</p>
<p>More newsletter signup prompts here.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("rust ownership borrowing".into()),
            ..Default::default()
        },
    );
    assert!(
        r.markdown.contains("dropped by focus:"),
        "manifest missing: {}",
        r.markdown
    );
    assert!(r.markdown.contains("blocks"));
}

#[test]
fn v3_dropped_manifest_absent_without_focus() {
    let html = r#"<html><body><article>
<h1>Guide</h1><p>Some content here.</p><p>More content.</p>
</article></body></html>"#;
    let r = extract_html_opts(html, &ExtractOptions::default());
    assert!(!r.markdown.contains("dropped by focus:"));
}

// ── v3: toc section IDs + sizes, section by ID ───────────────

// ── v3: probe mode ───────────────────────────────────────────

/// Issue #49: links and formatting nested inside em/strong were
/// flattened away by `plain()`. Nested inline markup must survive:
/// `<em>A <strong><a>B</a></strong> C</em>` → `*A **[B](url)** C*`.
#[test]
fn v3_nested_inline_formatting_preserves_links() {
    let html = r#"<html><body><article>
<p>
<em>A <strong><a href="https://example.com/b">B</a></strong> C</em>
</p>
<p>
<em>D <strong>E</strong> F</em>
</p>
<p>
<em>G <a href="https://example.com/h">H</a> I</em>
</p>
<p>
<strong><a href="https://example.com/j">J</a></strong>
</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            include_links: true,
            ..Default::default()
        },
    );
    let m = &r.markdown;
    // Case A: em > [strong > link]. Nested bold + link survive.
    assert!(
        m.contains("*A **[B](https://example.com/b)** C*"),
        "A: {}",
        m
    );
    // Case D: em > strong. Bold survives inside emphasis.
    assert!(m.contains("*D **E** F*"), "D: {}", m);
    // Case G: em > link. Link survives inside emphasis.
    assert!(m.contains("*G [H](https://example.com/h) I*"), "G: {}", m);
    // Case J: strong > link.
    assert!(m.contains("**[J](https://example.com/j)**"), "J: {}", m);
}

/// Issue #227 (2026-09-15): pages whose text layout is bare text
/// separated by <br> tags (no <p> elements) came back as one single
/// flattened paragraph on the tier-2 ghost path (and every other
/// path: loose_text is the shared conversion site). The exact source
/// sequence from the live repro (69shuba): emsp text, `<br />` plus
/// \r, a second `<br />` plus \r\n. A br/whitespace/br run must
/// yield a paragraph break (blank line), matching browser rendering.
/// loose_text used to trim the br sentinel away; the direct br
/// sentinel push plus the shared collapse_to_markdown post-processing
/// are covered here.
#[test]
fn br_paragraphs_survive_across_paths() {
    // 1. The exact byte sequence reported in #227 (CDP outerHTML
    //    form: self-closing <br />, \r separators).
    let html = "<html><body><div class=\"txtnav\">\r\n&emsp;&emsp;Paragraph one text<br />\r\n<br />\r\n&emsp;&emsp;Paragraph two text<br />\r\n<br /></div></body></html>";
    let m = extract_html(html).markdown;
    assert!(
        m.contains("Paragraph one text\n\nParagraph two text"),
        "CDP form must produce a paragraph break (blank line), got: {m:?}"
    );

    // 2. Plain form (`<br>` + \n). Same contract.
    let html2 = "<html><body><div>&emsp;&emsp;Paragraph one text<br>\n<br>\n&emsp;&emsp;Paragraph two text<br>\n<br></div></body></html>";
    let m2 = extract_html(html2).markdown;
    assert!(
        m2.contains("Paragraph one text\n\nParagraph two text"),
        "plain form must keep paragraph structure, got: {m2:?}"
    );
}

#[test]
fn br_single_line_break_and_downstream() {
    // A single br inside flowing text = one newline.
    let m = extract_html("<html><body><div>alpha<br>beta</div></body></html>").markdown;
    assert!(
        m.contains("alpha\nbeta"),
        "single br = one newline, got: {m:?}"
    );

    // Downstream excerpt renderers inherit the structure: focus
    // receives the same converted text, breaks intact.
    let html = "<html><body><div>&emsp;&emsp;Paragraph one text<br />\r\n<br />\r\n&emsp;&emsp;Paragraph two text<br />\r\n<br /></div></body></html>";
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            focus: Some("paragraph".into()),
            ..Default::default()
        },
    );
    assert!(
        r.markdown.contains("Paragraph one text\n"),
        "focus path must inherit paragraph breaks, got: {:?}",
        r.markdown
    );
}

/// Issue #227 class, fallback path: the last-resort walker used to
/// flush a paragraph at EVERY br (splitting prose at every line
/// break) and add a space after each br's newline. Now one br = a
/// line break inside the paragraph, two in a row = the paragraph
/// break, matching the main converter and browser rendering.
#[test]
fn fallback_br_line_and_paragraph_breaks() {
    let long = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua ut enim ad minim veniam quis nostrud exercitation ullamco laboris nisi ut aliquip";
    let html = format!(
        "<html><body><div>{long}<br>{long} beta</div><div>{long} one<br><br>{long} two</div></body></html>",
    );
    let meta = metadata::metadata(&Html::parse_fragment("<div></div>"));
    let r = fallback::text_fallback(
        &html,
        &meta,
        "https://x.invalid",
        &Default::default(),
        10_000,
    )
    .expect("fallback must extract");
    let m = &r.markdown;
    assert!(
        m.contains("aliquip\nlorem"),
        "single br = one newline, got: {m:?}"
    );
    assert!(
        m.contains("one\n\nlorem"),
        "br/blank/br = paragraph break, got: {m:?}"
    );
    assert!(
        !m.contains("\n "),
        "no leading space after any br newline, got: {m:?}"
    );
}

// The fetch tool parsed a PDF inline on the tokio worker; the crawl
// used the blocking pool. One helper now serves both: a PDF body is
// parsed off the worker under a budget, HTML goes straight through.
#[tokio::test]
async fn extract_off_worker_parses_pdf_off_the_runtime_and_html_inline() {
    let html = b"<html><head><title>T</title></head><body><article><p>Hello there, a paragraph long enough to keep.</p></article></body></html>";
    let opts = ExtractOptions::default();
    let out = super::extract_off_worker(html, "text/html", "https://ex.com/", &opts)
        .await
        .expect("html");
    assert!(out.markdown.contains("Hello there"));
    // A PDF body: routed through spawn_blocking; pdfium rejects the
    // truncated bytes, and the error must come back typed, not as a
    // join panic.
    let pdf = b"%PDF-1.4\ngarbage";
    let err = super::extract_off_worker(pdf, "application/pdf", "https://ex.com/x.pdf", &opts)
        .await
        .err();
    // Either a typed failure or, on a lenient pdfium, an empty doc;
    // never a panic.
    let _ = err;
}

#[test]
fn wave450_adapter_token_estimate_measures_the_returned_slice() {
    let body = "real post body with details. ".repeat(100);
    let html = format!(
        "<html><body><div class='thing link' data-author='a' data-score='5'><a class='title'>A real post</a><div class='usertext-body'><div class='md'><p>{body}</p></div></div></div></body></html>"
    );
    let ex = super::reddit::extract(
        &html,
        "https://old.reddit.com/r/rust/",
        &ExtractOptions {
            max_chars: Some(200),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ex.next_offset.is_some());
    assert_eq!(ex.tokens_est, ex.markdown.len() / 4);
}

#[test]
fn report_audit_pagination_caps_markers_and_reassembles_unicode_exactly() {
    let text = format!(
        "{}END\n\n{}",
        "日本語 evidence ".repeat(70),
        "tail ".repeat(60)
    );
    let mut offset = 0;
    let mut rebuilt = String::new();
    for _ in 0..100 {
        let (slice, next) = paginate_public(&text, offset, 200);
        assert!(slice.len() <= 200, "marker must fit the byte budget");
        if let Some(next) = next {
            assert!(next > offset && text.is_char_boundary(next));
            rebuilt.push_str(slice.split("\n\n*[truncated : continue").next().unwrap());
            offset = next;
        } else {
            rebuilt.push_str(&slice);
            break;
        }
    }
    assert_eq!(
        rebuilt, text,
        "continuations cannot omit or repeat evidence"
    );
}

#[test]
fn report_audit_tiny_pagination_never_overruns_or_loses_unicode() {
    let text = "日本語 😀 evidence";
    for cap in 4..60 {
        let mut offset = 0;
        let mut rebuilt = String::new();
        loop {
            let (slice, next) = paginate_public(text, offset, cap);
            assert!(slice.len() <= cap);
            rebuilt.push_str(slice.split("\n\n*[truncated : continue").next().unwrap());
            match next {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(rebuilt, text);
    }
    assert_eq!(paginate_public(text, 0, 0), (String::new(), Some(0)));
}

#[test]
fn report_audit_focus_fallback_retains_lead_table_and_source_order() {
    let html = r#"<html><title>Company</title><script type="application/ld+json">{"author":{"name":"Contributors to Wikimedia projects"},"datePublished":"2006-08-01"}</script><body><article><p>Company founding funding valuation lead describes the company.</p><table><tr><th>Founder</th><th>Year</th></tr><tr><td>Ada</td><td>2021</td></tr><tr><td>Bob</td><td>2022</td></tr></table><h2>Funding</h2><p>Company founding funding valuation funding round one.</p><h2>History</h2><p>Company founding funding valuation history passage.</p></article></body></html>"#;
    let options = ExtractOptions {
        focus: Some("company founding funding valuation".into()),
        ..Default::default()
    };
    let ex = extract(
        html.as_bytes(),
        "text/html",
        "https://en.wikipedia.org/wiki/Company",
        &options,
    )
    .unwrap();
    assert!(ex.markdown.contains("less than 20% saving"));
    assert!(!ex.markdown.contains("Contributors to Wikimedia"));
    for (earlier, later) in [
        ("lead describes", "Ada"),
        ("Ada", "round one"),
        ("round one", "history passage"),
    ] {
        assert!(ex.markdown.find(earlier).unwrap() < ex.markdown.find(later).unwrap());
    }
    let other = extract(
        html.as_bytes(),
        "text/html",
        "https://example.com/",
        &options,
    )
    .unwrap();
    assert!(
        other.markdown.contains("Contributors to Wikimedia"),
        "host-specific metadata must not remove other authors"
    );
}

#[test]
fn reliability_links_survive_loose_content_headings_and_table_cells() {
    let html = r#"<html><body><main><h1>Evidence</h1>
    <div>Research cites <a href="https://loose.example/report">the original report</a> for verification and factual context.</div>
    <h2><a href="https://heading.example/spec">Specification source</a></h2>
    <p>The evidence has ample descriptive content, including <a href="https://prose.example/data">the official dataset</a> with verifiable results.</p>
    <table><tr><th>Source</th><th>Claim</th></tr><tr><td><a href="https://table.example/study">Original study</a></td><td>Verified data</td></tr></table>
    </main></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            include_links: true,
            ..Default::default()
        },
    );
    for host in ["loose", "heading", "prose", "table"] {
        assert!(
            r.markdown.contains(&format!("https://{host}.example/")),
            "{host}: {}",
            r.markdown
        );
    }
}

#[test]
fn reliability_link_projection_does_not_change_source_fingerprint() {
    let html = r#"<main><h1>Evidence</h1><p>This research links to <a href='https://source.example/spec'>the specification</a> for complete supporting evidence and context.</p><div>Loose <a href='https://other.example/data'>data source</a> is useful too.</div></main>"#;
    let plain = extract_html_opts(html, &ExtractOptions::default());
    let links = extract_html_opts(
        html,
        &ExtractOptions {
            include_links: true,
            ..Default::default()
        },
    );
    assert_eq!(plain.fingerprint, links.fingerprint);
    assert_ne!(plain.markdown, links.markdown);
}

#[test]
fn reliability_short_article_focus_preserves_specific_evidence() {
    let html = format!(
        "<main><h1>Deployment handbook</h1><h2>Retries</h2><p>On HTTP 503, retry with exponential backoff and jitter; honor Retry-After.</p><h2>Storage</h2><p>{}</p><h2>Logging</h2><p>{}</p></main>",
        "Storage retains encrypted snapshots. ".repeat(22),
        "Logging records errors without tokens. ".repeat(22)
    );
    let full = extract_html_opts(&html, &ExtractOptions::default());
    let focused = extract_html_opts(
        &html,
        &ExtractOptions {
            focus: Some("HTTP 503 retry backoff".into()),
            ..Default::default()
        },
    );
    assert!(focused.markdown.contains("Retry-After"));
    assert!(!focused.markdown.contains("showing full content"));
    assert!(focused.markdown.len() * 2 < full.markdown.len());
}

#[test]
fn a_wall_fails_extraction_whatever_the_window() {
    // fetch's views run on a successful extraction only, so the wall
    // verdict comes first for any view; a tiny window cannot hide it.
    let html = "<html><head><title>Blocked</title></head><body><main><h1>You've been blocked by network security.</h1><p>To continue, log in to your Reddit account or use your developer token.</p></main></body></html>";
    for opts in [
        ExtractOptions::default(),
        ExtractOptions {
            max_chars: Some(200),
            offset: 150,
            ..Default::default()
        },
    ] {
        assert!(
            extract(
                html.as_bytes(),
                "text/html",
                "https://old.reddit.com/r/rust/",
                &opts
            )
            .is_err(),
            "a wall must fail before pagination"
        );
    }
    let article = format!("<article><h1>Network security guide</h1>{}<p>The phrase You've been blocked by network security appears on Reddit's block screen.</p></article>", "<p>This guide explains secure connections, authentication and debugging with real application examples.</p>".repeat(25));
    assert!(
        extract(
            article.as_bytes(),
            "text/html",
            "https://example.com",
            &ExtractOptions::default()
        )
        .is_ok()
    );
}

#[test]
fn media_is_recorded_for_ocr_but_never_rendered() {
    let html = r#"<html><body><article>
<p>The system has three services that talk over a queue, described below in full detail for operators.</p>
<figure>
<img src="https://example.com/diagram.png" alt="Architecture diagram">
<figcaption>System architecture overview</figcaption>
</figure>
</article></body></html>"#;
    let r = extract_html(html);
    assert!(!r.markdown.contains("diagram.png"), "{}", r.markdown);
    assert!(
        r.images.iter().any(|(_, src)| src.ends_with("diagram.png")),
        "{:?}",
        r.images
    );
}

// ── Markdown structure survives hostile metadata and cell text ──

#[test]
fn a_multiline_meta_description_stays_one_quoted_line() {
    // A content attribute carries newlines happily; emitted raw, the
    // summary breaks out of its blockquote and the rest reads as the
    // page's own prose.
    let html = "<html><head><title>Release notes</title>\
        <meta name=\"description\" content=\"First line of the summary.\nSecond line, which used to escape the quote.\n\">\
        </head><body><article><h1>Release notes</h1>\
        <p>The release adds a retry policy, a bounded queue and a per-host budget, all documented below.</p>\
        </article></body></html>";
    let md = extract_html(html).markdown;
    assert!(
        md.contains("> First line of the summary. Second line, which used to escape the quote."),
        "summary not folded onto one quoted line:\n{md}"
    );
    assert!(
        !md.lines()
            .any(|l| l.trim() == "Second line, which used to escape the quote."),
        "summary escaped its blockquote:\n{md}"
    );
}

#[test]
fn a_table_cell_pipe_cannot_shift_columns() {
    let html = r#"<html><body><article>
<table>
<tr><th>Flag</th><th>Meaning</th></tr>
<tr><td>a|b</td><td>either, as written</td></tr>
<tr><td>--x</td><td>exclusive</td></tr>
</table>
</article></body></html>"#;
    let md = extract_html(html).markdown;
    let row = md
        .lines()
        .find(|l| l.starts_with("| a"))
        .unwrap_or_else(|| panic!("row missing:\n{md}"));
    let delimiters = row.matches('|').count() - row.matches("\\|").count();
    assert_eq!(delimiters, 3, "column count shifted: {row}");
    assert!(row.contains("a\\|b"), "pipe not escaped: {row}");
}

#[test]
fn a_table_cell_with_a_newline_is_one_cell() {
    let html = r#"<html><body><article>
<table>
<tr><th>Flag</th><th>Meaning</th></tr>
<tr><td>--verbose</td><td>print every
hop of the request</td></tr>
<tr><td>--quiet</td><td>say nothing</td></tr>
</table>
</article></body></html>"#;
    let md = extract_html(html).markdown;
    let rows: Vec<&str> = md.lines().filter(|l| l.starts_with("|")).collect();
    assert_eq!(rows.len(), 4, "a cell newline split the row:\n{md}");
    assert!(
        rows[2].contains("print every hop of the request"),
        "{}",
        rows[2]
    );
}

#[test]
fn a_highlighter_wrapper_class_names_the_fence() {
    // GitHub marks the language on the div that wraps the <pre>.
    let html = r#"<html><body><article><h2>Example</h2>
<div class="highlight highlight-source-rust notranslate"><pre>fn main() { println!("hi"); }</pre></div>
</article></body></html>"#;
    let md = extract_html(html).markdown;
    assert!(md.contains("```rust\n"), "wrapper language lost:\n{md}");
    assert!(
        !md.contains("```source-rust") && !md.contains("```highlight"),
        "prefix stripped wrong:\n{md}"
    );
}

const AUDIT_PAD: &str = "<p>This paragraph is ordinary article prose that exists only so the scorer sees a real article body with enough text, punctuation, and paragraphs to win.</p><p>A second distinct paragraph of filler prose, again present only so that the article container clearly wins the main-content scoring pass.</p>";

fn audit_page(body: &str) -> String {
    format!(
        "<!doctype html><html><head><title>Audit</title></head><body><article><h1>Audit</h1>{AUDIT_PAD}{body}</article></body></html>"
    )
}

// <figure> is walked only for its images: a table, code listing or
// quote wrapped in one is dropped. WordPress wraps every table block in
// <figure class="wp-block-table">; Jekyll's highlight tag emits
// <figure class="highlight"><pre>.
#[test]
fn audit_figure_wrapped_table_code_and_quote_are_kept() {
    let html = audit_page(
        r#"<h2>Pricing</h2><figure class="wp-block-table"><table><thead><tr><th>Plan</th><th>Price</th></tr></thead><tbody><tr><td>Basic</td><td>$5 per month</td></tr><tr><td>Pro</td><td>$25 per month</td></tr></tbody></table></figure>
<h2>Code</h2><figure class="highlight"><pre><code class="language-ruby">def hello
  puts "UNIQUE_CODE_LINE"
end</code></pre></figure>
<h2>Quote</h2><figure class="wp-block-pullquote"><blockquote><p>UNIQUE_QUOTE_TEXT is what the speaker said.</p></blockquote></figure>"#,
    );
    let md = extract_html(&html).markdown;
    for needle in ["$25 per month", "UNIQUE_CODE_LINE", "UNIQUE_QUOTE_TEXT"] {
        assert!(md.contains(needle), "lost {needle:?}:\n{md}");
    }
}

// Negative for the figure walk: a figure holding only images and a
// caption stays a media block. Bare, linked and <picture> images each
// yield one image with the caption as alt, and no caption paragraph
// reaches the output; a gallery of nested figures does not turn its
// captions into paragraphs either.
#[test]
fn image_only_figures_stay_on_the_media_path() {
    let html = audit_page(
        r#"<h2>Photos</h2>
<figure><img src="/bare.png"><figcaption>BARE_CAPTION</figcaption></figure>
<figure><a href="/full.png"><img src="/linked.png"></a><figcaption>LINKED_CAPTION</figcaption></figure>
<figure><picture><source srcset="/pic.webp" type="image/webp"><img src="/pic.png"></picture><figcaption>PICTURE_CAPTION</figcaption></figure>
<figure class="wp-block-gallery"><figure><img src="/g1.png" alt="first"><figcaption>G1_CAPTION</figcaption></figure><figure><img src="/g2.png" alt="second"></figure><figcaption>GALLERY_CAPTION</figcaption></figure>"#,
    );
    let ex = extract_html(&html);
    let img = |alt: &str, path: &str| (alt.to_string(), format!("https://example.com{path}"));
    assert_eq!(
        ex.images,
        vec![
            img("BARE_CAPTION", "/bare.png"),
            img("LINKED_CAPTION", "/linked.png"),
            img("PICTURE_CAPTION", "/pic.png"),
            img("first", "/g1.png"),
            img("second", "/g2.png"),
        ]
    );
    assert!(!ex.markdown.contains("_CAPTION"), "{}", ex.markdown);
}

// A figure that wraps content and a linked image: the image is recorded
// once (not by both the media path and the walk), and the caption is
// kept as text next to the table it describes.
#[test]
fn content_figure_emits_each_image_once_and_keeps_its_caption() {
    let html = audit_page(
        r#"<h2>Results</h2><figure><a href="/big.png"><img src="/chart.png" alt="Chart"></a><table><tr><th>Run</th><th>Score</th></tr><tr><td>First</td><td>UNIQUE_SCORE_ONE</td></tr><tr><td>Second</td><td>UNIQUE_SCORE_TWO</td></tr></table><figcaption>TABLE_CAPTION</figcaption></figure>"#,
    );
    let ex = extract_html(&html);
    assert_eq!(
        ex.images,
        vec![(
            "Chart".to_string(),
            "https://example.com/chart.png".to_string()
        )]
    );
    for needle in ["UNIQUE_SCORE_TWO", "TABLE_CAPTION"] {
        assert!(
            ex.markdown.contains(needle),
            "lost {needle:?}:\n{}",
            ex.markdown
        );
    }
}
