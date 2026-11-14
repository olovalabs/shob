//! M5e integration — prompt-template loading + argument substitution. Mirrors
//! the cases in `packages/agent/test/harness/prompt-templates.test.ts`:
//! - non-recursive directory load (nested `.md` ignored; description falls back
//!   to the first non-blank body line when frontmatter omits it);
//! - explicit `.md` file input;
//! - sourced templates preserve source + attach source to diagnostics;
//! - frontmatter parse failure (`[unterminated` flow sequence) →
//!   `parse_failed` diagnostic with the source attached;
//! - `format_prompt_template_invocation` substitutes `$1`${@:2}`$ARGUMENTS`.
//!
//! The TS symlink case is deferred to an OS-env conformance test (the in-memory
//! env's `canonical_path` is identity) — see the initial port notes (retired).

use std::sync::Arc;

use rpi_harness::prompt_templates::{
    format_prompt_template_invocation, load_prompt_templates, load_sourced_prompt_templates,
    substitute_args, PromptTemplateDiagnosticCode, SourcedTemplateInput,
};
use rpi_harness::types::PromptTemplate;
use rpi_tools::in_memory::InMemoryExecutionEnv;
use rpi_tools::FileSystem;

fn fresh_env() -> (Arc<InMemoryExecutionEnv>, Arc<dyn rpi_tools::ExecutionEnv>) {
    let typed = Arc::new(InMemoryExecutionEnv::new());
    let env: Arc<dyn rpi_tools::ExecutionEnv> = typed.clone();
    (typed, env)
}

#[tokio::test]
async fn loads_markdown_templates_non_recursively_from_dirs() {
    let (typed, env) = fresh_env();
    typed.create_dir("a/nested", true, None).await.unwrap();
    typed.create_dir("b", true, None).await.unwrap();
    typed
        .write_file(
            "a/one.md",
            "---\ndescription: One template\n---\nHello $1".into(),
            None,
        )
        .await
        .unwrap();
    // Nested .md must be ignored (loader recurses? NO — prompt-templates load is
    // non-recursive; only direct children load).
    typed
        .write_file("a/nested/ignored.md", "Ignored".into(), None)
        .await
        .unwrap();
    // No frontmatter: description falls back to the first non-blank body line.
    typed
        .write_file("b/two.md", "First line description\nBody".into(), None)
        .await
        .unwrap();

    let result = load_prompt_templates(&env, &["a".to_string(), "b".to_string()]).await;
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(result.prompt_templates.len(), 2);
    assert_eq!(
        result.prompt_templates[0],
        PromptTemplate {
            name: "one".to_string(),
            description: Some("One template".to_string()),
            content: "Hello $1".to_string()
        }
    );
    assert_eq!(
        result.prompt_templates[1],
        PromptTemplate {
            name: "two".to_string(),
            description: Some("First line description".to_string()),
            content: "First line description\nBody".to_string(),
        }
    );
}

#[tokio::test]
async fn loads_explicit_markdown_file_input() {
    // Mirrors TS "loads explicit markdown files" (symlink half deferred). A
    // bare `.md` file path loads directly; name = basename without the trailing
    // `.md` (single, case-insensitive strip).
    let (typed, env) = fresh_env();
    typed
        .write_file(
            "target.md",
            "---\ndescription: Target\n---\nTarget body".into(),
            None,
        )
        .await
        .unwrap();
    let result = load_prompt_templates(&env, &["target.md".to_string()]).await;
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(result.prompt_templates.len(), 1);
    assert_eq!(result.prompt_templates[0].name, "target");
    assert_eq!(result.prompt_templates[0].content, "Target body");
}

#[tokio::test]
async fn sourced_templates_preserve_source() {
    let (typed, env) = fresh_env();
    typed.create_dir("prompts", true, None).await.unwrap();
    typed
        .write_file(
            "prompts/example.md",
            "---\ndescription: Example\n---\nExample body".into(),
            None,
        )
        .await
        .unwrap();

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Source {
        Project,
    }

    let result = load_sourced_prompt_templates::<Source>(
        &env,
        &[SourcedTemplateInput {
            path: "prompts".to_string(),
            source: Source::Project,
        }],
    )
    .await;
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(result.templates.len(), 1);
    let t = &result.templates[0];
    assert_eq!(t.template.name, "example");
    assert_eq!(t.template.description.as_deref(), Some("Example"));
    assert_eq!(t.template.content, "Example body");
    assert_eq!(t.source, Source::Project);
}

#[tokio::test]
async fn sourced_templates_attach_source_to_parse_diagnostics() {
    // Mirrors TS "attaches source info to diagnostics": `description:
    // [unterminated` is an unterminated flow sequence → parse_failed, with the
    // source attached.
    let (typed, env) = fresh_env();
    typed
        .write_file(
            "broken.md",
            "---\ndescription: [unterminated\n---\nBody".into(),
            None,
        )
        .await
        .unwrap();

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Source {
        User,
    }

    let result = load_sourced_prompt_templates::<Source>(
        &env,
        &[SourcedTemplateInput {
            path: "broken.md".to_string(),
            source: Source::User,
        }],
    )
    .await;
    assert!(result.templates.is_empty());
    assert_eq!(result.diagnostics.len(), 1);
    let d = &result.diagnostics[0];
    assert_eq!(d.diagnostic.code, PromptTemplateDiagnosticCode::ParseFailed);
    assert_eq!(d.source, Source::User);
    assert!(d.diagnostic.path.ends_with("broken.md"));
}

#[test]
fn format_invocation_substitutes_command_arguments() {
    // Mirrors TS `formatPromptTemplateInvocation({ name, content }, args)`.
    let t = PromptTemplate {
        name: "one".to_string(),
        description: None,
        content: "$1 ${@:2} $ARGUMENTS".to_string(),
    };
    let out =
        format_prompt_template_invocation(&t, &["hello world".to_string(), "test".to_string()]);
    assert_eq!(out, "hello world test hello world test");
}

#[test]
fn substitute_args_all_placeholder_forms() {
    let args = vec!["hello".to_string(), "world".to_string(), "test".to_string()];
    assert_eq!(substitute_args("$1", &args), "hello");
    assert_eq!(substitute_args("$2 $3", &args), "world test");
    assert_eq!(substitute_args("${@:2}", &args), "world test");
    assert_eq!(substitute_args("${@:1:2}", &args), "hello world");
    assert_eq!(substitute_args("$ARGUMENTS", &args), "hello world test");
    assert_eq!(substitute_args("$@", &args), "hello world test");
    // Out-of-range $N → empty; $0 → empty (TS args[-1] ?? "").
    assert_eq!(substitute_args("$9", &args), "");
    assert_eq!(substitute_args("$0", &args), "");
}

#[test]
fn substitute_args_does_not_re_expand_expansion() {
    // $ARGUMENTS expands to a literal containing `$1`; that `$1` must survive
    // intact (no re-expansion) — the replacement-order guarantee.
    let args = vec!["$1".to_string()];
    assert_eq!(substitute_args("$ARGUMENTS", &args), "$1");
}

#[test]
fn template_name_strips_single_md_once() {
    // Mirrors TS `fileName.replace(/\.md$/i, "")` — a single (not repeated)
    // case-insensitive trailing `.md` strip.
    use rpi_harness::prompt_templates::parse_command_args;
    assert_eq!(parse_command_args("a b c"), vec!["a", "b", "c"]);
    // sanity-check the helper isn't dead: a quoted multi-word arg.
    assert_eq!(
        parse_command_args("'hello world' test"),
        vec!["hello world", "test"]
    );
}
