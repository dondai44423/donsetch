//! Comprehensive integration tests for DonSift : the
//! extraction engine. Tests the full pipeline: HTML bytes in
//! → agent-ready markdown out. Every language, every block
//! type, every edge case. This is what makes DonSift
//! standalone-quality.

use super::language::{self, Script};
use super::metadata;
use super::*;
use scraper::Html;

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
    assert_eq!(info.code, "ko");
    assert_eq!(info.script, Script::Hangul);
}

#[test]
fn lang_detect_mixed_japanese() {
    let doc = Html::parse_document(
        "<html><body>これは日本語の文章です。機械学習について説明します。</body></html>",
    );
    let info = language::detect(&doc);
    assert_eq!(info.code, "ja");
    assert!(info.scripts.contains(&Script::Kana));
    assert!(info.scripts.contains(&Script::Han));
}

#[test]
fn lang_detect_default_english() {
    let doc = Html::parse_document("<html><body></body></html>");
    let info = language::detect(&doc);
    assert_eq!(info.code, "en");
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
    assert!(!r.markdown.contains("*[focus"));
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
    assert!(!r.markdown.contains("*[focus"));
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
    assert!(!r.markdown.contains("*[focus"));
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
    assert!(!r.markdown.contains("*[focus"));
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
    assert!(!r.markdown.contains("*[focus"));
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
fn block_media_opt_in() {
    let html = r#"<html><body><article>
<figure>
<img src="/diagram.png" alt="Architecture diagram">
<figcaption>System architecture overview</figcaption>
</figure>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            include_media: true,
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("Architecture diagram"));
    // Default (no media): should not include.
    let r2 = extract_html(html);
    assert!(!r2.markdown.contains("diagram.png"));
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

#[test]
fn toc_mode_heading_tree() {
    let html = r#"<html><body><article>
<h1>Main Title</h1>
<p>Intro paragraph.</p>
<h2>Section A</h2>
<p>Section A content.</p>
<h3>Subsection A1</h3>
<p>Subsection content.</p>
<h2>Section B</h2>
<p>Section B content.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            toc: true,
            ..Default::default()
        },
    );
    // v3: toc lines carry stable section IDs + size labels.
    assert!(r.markdown.contains("- [s1] Main Title ·"));
    assert!(r.markdown.contains("  - [s2] Section A ·"));
    assert!(r.markdown.contains("    - [s3] Subsection A1 ·"));
    assert!(r.markdown.contains("  - [s4] Section B ·"));
    assert!(r.markdown.contains("section=\"sN\""));
    // Should NOT contain body text.
    assert!(!r.markdown.contains("Intro paragraph"));
    assert!(!r.markdown.contains("Section A content"));
}

#[test]
fn toc_mode_flat_page() {
    let html = r#"<html><body><article>
<p>No headings here, just paragraphs.</p>
<p>Another paragraph.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            toc: true,
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("no headings"));
}

#[test]
fn toc_mode_is_not_replaced_by_raw_text_fallback() {
    let body = "This paragraph belongs to the page body, not to its requested outline. ".repeat(20);
    let html = format!(
        r#"<html><head><title>Small guide</title></head><body><article>
<h1>Small guide</h1><p>{body}</p>
</article></body></html>"#
    );
    let r = extract_html_opts(
        &html,
        &ExtractOptions {
            toc: true,
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("[s1] Small guide"), "{}", r.markdown);
    assert!(!r.markdown.contains("This paragraph belongs"));
}

// ════════════════════════════════════════════════════════════
// 9. SECTION MODE
// ════════════════════════════════════════════════════════════

#[test]
fn section_mode_exact_match() {
    let html = r#"<html><body><article>
<h1>Document</h1>
<p>Intro.</p>
<h2>History</h2>
<p>The history of the subject is long and complex.</p>
<h3>Early Period</h3>
<p>Early details about the subject.</p>
<h2>Modern Era</h2>
<p>Modern developments in the field.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            section: Some("History".to_string()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("history of the subject"));
    assert!(r.markdown.contains("Early details"));
    // Should NOT include content from "Modern Era" section.
    assert!(!r.markdown.contains("Modern developments"));
}

#[test]
fn section_mode_case_insensitive() {
    let html = r#"<html><body><article>
<h2>Installation</h2>
<p>To install, run the command.</p>
<h2>Usage</h2>
<p>How to use the tool.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            section: Some("INSTALLATION".to_string()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("install, run"));
    assert!(!r.markdown.contains("How to use"));
}

#[test]
fn section_mode_miss_returns_full() {
    let html = r#"<html><body><article>
<h2>Real Section</h2>
<p>Real content.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            section: Some("Nonexistent".to_string()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("*[section"));
    assert!(r.markdown.contains("Real content"));
}

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
    let html = r#"<html><body><article>
<p>Duplicate content that appears multiple times.</p>
<p>Duplicate content that appears multiple times.</p>
<p>Unique content that only appears once.</p>
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
fn signal_section_miss() {
    let html = r#"<html><body><article>
<h2>Real Section</h2>
<p>Real content here.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            section: Some("Nonexistent Section".to_string()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("*[section"));
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

// ════════════════════════════════════════════════════════════
// 16. CSS SELECTOR
// ════════════════════════════════════════════════════════════

#[test]
fn css_selector_scopes_extraction() {
    let html = r#"<html><body>
<div class="sidebar"><p>Sidebar junk.</p></div>
<div class="main-content">
<p>This is the content we want to extract from the main div.</p>
</div>
</body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            selector: Some(".main-content".to_string()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("content we want"));
    assert!(!r.markdown.contains("Sidebar junk"));
}

#[test]
fn css_selector_bad_returns_error() {
    let r = extract(
        "<html></html>".as_bytes(),
        "text/html",
        "https://example.com",
        &ExtractOptions {
            selector: Some("invalid_selector[[[".to_string()),
            ..Default::default()
        },
    );
    assert!(r.is_err());
}

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
    assert!(!r.markdown.contains("*[focus"));
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
    assert!(!r.markdown.contains("*[focus"));
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

// ════════════════════════════════════════════════════════════
// 24. TABLE WITHOUT <th> : FIRST ROW PROMOTED TO HEADERS
// ════════════════════════════════════════════════════════════

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

#[test]
fn v3_section_by_id_targets_nth_heading() {
    let html = r#"<html><body><article>
<h1>Main</h1><p>intro</p>
<h2>Alpha</h2><p>alpha content</p>
<h2>Beta</h2><p>beta content marker_zq1</p>
</article></body></html>"#;
    // s3 = "Beta" (1-based across ALL headings in order)
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            section: Some("s3".into()),
            ..Default::default()
        },
    );
    assert!(
        r.markdown.contains("beta content marker_zq1"),
        "section-by-id missed: {}",
        r.markdown
    );
    assert!(!r.markdown.contains("alpha content"));
}

#[test]
fn v3_section_by_name_still_works() {
    let html = r#"<html><body><article>
<h1>Main</h1><p>intro</p>
<h2>Alpha</h2><p>alpha content</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            section: Some("alpha".into()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("alpha content"));
}

// ── v3: probe mode ───────────────────────────────────────────

#[test]
fn v3_probe_substring_match_and_miss() {
    let html = r#"<html><body><article>
<h1>Advisory</h1>
<p>The CVE-2026-1234 vulnerability was patched in version 2.3.1.</p>
<p>Unrelated paragraph about weather and sunshine.</p>
</article></body></html>"#;
    let base = ExtractOptions {
        must_contain: Some("cve-2026-1234".into()),
        ..Default::default()
    };
    let r = extract_html_opts(html, &base);
    assert!(r.markdown.starts_with("probe: MATCH"), "{}", r.markdown);
    assert!(r.markdown.contains("[1]"), "{}", r.markdown);
    assert!(
        r.markdown.len() < 400,
        "probe output too big: {}",
        r.markdown.len()
    );
    // No-hit case
    let miss = extract_html_opts(
        html,
        &ExtractOptions {
            must_contain: Some("definitely-not-there".into()),
            ..Default::default()
        },
    );
    assert!(
        miss.markdown.starts_with("probe: NO MATCH"),
        "{}",
        miss.markdown
    );
}

#[test]
fn v3_probe_regex_mode() {
    let html = r#"<html><body><article>
<p>Fixed CVE-2026-1111 and CVE-2026-9999 in this release.</p>
</article></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            must_contain: Some("/CVE-2026-\\d{4}/".into()),
            ..Default::default()
        },
    );
    assert!(
        r.markdown.starts_with("probe: MATCH : 2 hits"),
        "{}",
        r.markdown
    );
}

#[test]
fn v3_probe_invalid_regex_is_honest_not_panic() {
    let html = r#"<html><body><p>text</p></body></html>"#;
    let r = extract_html_opts(
        html,
        &ExtractOptions {
            must_contain: Some("/([unclosed/".into()),
            ..Default::default()
        },
    );
    assert!(r.markdown.contains("invalid regex"), "{}", r.markdown);
}
#[test]
fn must_contain_regex_with_trailing_flags_is_still_regex() {
    // A trailing flag like /i must not downgrade the probe to a
    // literal search.
    let hit = extract_html_opts(
        "cve-2026-1234 hatchling",
        &ExtractOptions {
            must_contain: Some("/CVE-2026-\\d{4}/i".into()),
            ..Default::default()
        },
    );
    assert!(hit.markdown.contains("probe: MATCH"), "{}", hit.markdown);
    let miss = extract_html_opts(
        "nothing here",
        &ExtractOptions {
            must_contain: Some("/CVE-2026-\\d{4}/i".into()),
            ..Default::default()
        },
    );
    assert!(
        miss.markdown.contains("probe: NO MATCH"),
        "{}",
        miss.markdown
    );
}

#[test]
fn must_contain_probe_applies_to_plain_text_passthrough() {
    let body = b"HTTP/1.1 Semantics and Content\r\nFielding, Ed.\r\n";
    let out = extract(
        body,
        "text/plain",
        "https://example.com/rfc",
        &ExtractOptions {
            must_contain: Some("Fielding".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(out.markdown.contains("probe: MATCH"), "{}", out.markdown);
    // The probe must replace the body, not dump it: a probe block is
    // a handful of lines, the raw passthrough would carry the full
    // document text.
    assert!(out.markdown.len() < 400, "len {}", out.markdown.len());
}

// Hits were BYTE offsets (regex m.start(), or offsets into a
// separately lowercased haystack), but context_around treated them
// as CHAR indices. On any page with non-ASCII text before the match
// the excerpt window landed after the real hit -- "MATCH: 1 hit"
// followed by an excerpt that doesn't contain the pattern.
fn probe_excerpt(text: &str, pattern: &str) -> String {
    let opts = ExtractOptions {
        must_contain: Some(pattern.into()),
        ..Default::default()
    };
    let out = probe_render(text, opts.probe_pattern(), opts.probe_is_regex());
    assert!(out.starts_with("probe: MATCH"), "{out}");
    out.lines()
        .find(|l| l.starts_with("[1]"))
        .unwrap_or_else(|| panic!("no excerpt line:\n{out}"))
        .to_string()
}

#[test]
fn probe_excerpt_points_at_the_hit_on_non_ascii_pages() {
    let prefix = "Асинхронная среда выполнения для языка Rust. ".repeat(6);
    let text = format!("{prefix}The tokio runtime schedules tasks. {prefix}");
    let sub = probe_excerpt(&text, "tokio runtime");
    assert!(sub.contains("tokio runtime"), "substring excerpt: {sub}");
    let re = probe_excerpt(&text, "/tokio\\s+runtime/");
    assert!(re.contains("tokio runtime"), "regex excerpt: {re}");
}

#[test]
fn probe_excerpt_survives_case_folding_that_changes_byte_length() {
    // 'İ' (2 bytes) lowercases to "i̇" (3 bytes): the old lowercased
    // haystack drifted further from the real text with every one.
    let prefix = "İSTANBUL İZMİR İÇEL ".repeat(40);
    let text = format!("{prefix}Needle here. {prefix}");
    let sub = probe_excerpt(&text, "needle");
    assert!(sub.contains("Needle"), "excerpt: {sub}");
}

// A multi-KB substring pattern expands past the regex size limit
// under (?i); it must still be searched as a literal, not reported
// as an "invalid regex".
#[test]
fn probe_substring_longer_than_the_regex_size_limit_still_matches() {
    let needle = "проверка ".repeat(800); // ~14 KB, Cyrillic
    let text = format!("intro {needle} outro");
    let out = probe_render(&text, needle.trim_end(), false);
    assert!(
        out.starts_with("probe: MATCH : 1 hit"),
        "{}",
        &out[..out.len().min(120)]
    );
    let miss = probe_render("nothing here", needle.trim_end(), false);
    assert!(miss.starts_with("probe: NO MATCH"), "{miss}");
}

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
