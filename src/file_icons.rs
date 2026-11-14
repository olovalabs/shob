// File associations derived from Zed, GPL-3.0-or-later.
// See assets/zed_file_icons/NOTICE.md for source revision and licenses.
use std::path::Path;

const FILE_STEMS_BY_ICON_KEY: &[(&str, &[&str])] = &[
    ("docker", &["Containerfile", "Dockerfile", ".dockerignore"]),
    ("ruby", &["Podfile"]),
    ("heroku", &["Procfile"]),
];

const FILE_SUFFIXES_BY_ICON_KEY: &[(&str, &[&str])] = &[
    ("astro", &["astro"]),
    (
        "audio",
        &[
            "aac", "flac", "m4a", "mka", "mp3", "ogg", "opus", "wav", "wma", "wv",
        ],
    ),
    ("backup", &["bak"]),
    ("ballerina", &["bal"]),
    ("bicep", &["bicep"]),
    ("bun", &["lockb"]),
    ("c", &["c", "h"]),
    ("cairo", &["cairo"]),
    ("code", &["handlebars", "metadata", "rkt", "scm"]),
    ("coffeescript", &["coffee"]),
    (
        "cpp",
        &[
            "c++", "h++", "cc", "cpp", "cppm", "cxx", "hh", "hpp", "hxx", "inl", "ixx",
        ],
    ),
    ("crystal", &["cr", "ecr"]),
    ("csharp", &["cs"]),
    ("csproj", &["csproj"]),
    ("css", &["css", "pcss", "postcss"]),
    ("cue", &["cue"]),
    ("dart", &["dart"]),
    ("diff", &["diff"]),
    (
        "docker",
        &[
            "docker-compose.yml",
            "docker-compose.yaml",
            "compose.yml",
            "compose.yaml",
        ],
    ),
    (
        "document",
        &[
            "doc", "docx", "mdx", "odp", "ods", "odt", "pdf", "ppt", "pptx", "rtf", "txt", "xls",
            "xlsx",
        ],
    ),
    ("editorconfig", &["editorconfig"]),
    ("elixir", &["eex", "ex", "exs", "heex", "leex", "neex"]),
    ("elm", &["elm"]),
    (
        "erlang",
        &[
            "Emakefile",
            "app.src",
            "erl",
            "escript",
            "hrl",
            "rebar.config",
            "xrl",
            "yrl",
        ],
    ),
    (
        "eslint",
        &[
            "eslint.config.cjs",
            "eslint.config.cts",
            "eslint.config.js",
            "eslint.config.mjs",
            "eslint.config.mts",
            "eslint.config.ts",
            "eslintrc",
            "eslintrc.js",
            "eslintrc.json",
        ],
    ),
    ("font", &["otf", "ttf", "woff", "woff2"]),
    ("fsharp", &["fs"]),
    ("fsproj", &["fsproj"]),
    ("gitlab", &["gitlab-ci.yml", "gitlab-ci.yaml"]),
    ("gleam", &["gleam"]),
    ("go", &["go", "mod", "work"]),
    ("graphql", &["gql", "graphql", "graphqls"]),
    ("haskell", &["hs"]),
    ("hcl", &["hcl"]),
    (
        "helm",
        &[
            "helmfile.yaml",
            "helmfile.yml",
            "Chart.yaml",
            "Chart.yml",
            "Chart.lock",
            "values.yaml",
            "values.yml",
            "requirements.yaml",
            "requirements.yml",
            "tpl",
        ],
    ),
    ("html", &["htm", "html"]),
    (
        "image",
        &[
            "avif", "bmp", "gif", "heic", "heif", "ico", "j2k", "jfif", "jp2", "jpeg", "jpg",
            "jxl", "png", "psd", "qoi", "svg", "tiff", "webp",
        ],
    ),
    ("ipynb", &["ipynb"]),
    ("java", &["java"]),
    ("javascript", &["cjs", "js", "mjs"]),
    ("json", &["json", "jsonc"]),
    ("julia", &["jl"]),
    ("kdl", &["kdl"]),
    ("kotlin", &["kt"]),
    ("lock", &["lock"]),
    ("log", &["log"]),
    ("lua", &["lua"]),
    ("luau", &["luau"]),
    ("markdown", &["markdown", "md"]),
    ("metal", &["metal"]),
    ("nim", &["nim", "nims", "nimble"]),
    ("nix", &["nix"]),
    ("ocaml", &["ml", "mli", "mlx"]),
    ("odin", &["odin"]),
    ("php", &["php"]),
    (
        "prettier",
        &[
            "prettier.config.cjs",
            "prettier.config.js",
            "prettier.config.mjs",
            "prettierignore",
            "prettierrc",
            "prettierrc.cjs",
            "prettierrc.js",
            "prettierrc.json",
            "prettierrc.json5",
            "prettierrc.mjs",
            "prettierrc.toml",
            "prettierrc.yaml",
            "prettierrc.yml",
        ],
    ),
    ("prisma", &["prisma"]),
    ("puppet", &["pp"]),
    ("python", &["py"]),
    ("r", &["r", "R"]),
    ("react", &["cjsx", "ctsx", "jsx", "mjsx", "mtsx", "tsx"]),
    ("roc", &["roc"]),
    ("ruby", &["rb"]),
    ("rust", &["rs"]),
    ("sass", &["sass", "scss"]),
    ("scala", &["scala", "sc"]),
    ("settings", &["conf", "ini"]),
    ("solidity", &["sol"]),
    (
        "storage",
        &[
            "accdb", "csv", "dat", "db", "dbf", "dll", "fmp", "fp7", "frm", "gdb", "ib", "ldf",
            "mdb", "mdf", "myd", "myi", "pdb", "psv", "RData", "rdata", "sav", "sdf", "sql",
            "sqlite", "ssv", "tsv",
        ],
    ),
    (
        "stylelint",
        &[
            "stylelint.config.cjs",
            "stylelint.config.js",
            "stylelint.config.mjs",
            "stylelintignore",
            "stylelintrc",
            "stylelintrc.cjs",
            "stylelintrc.js",
            "stylelintrc.json",
            "stylelintrc.mjs",
            "stylelintrc.yaml",
            "stylelintrc.yml",
        ],
    ),
    ("surrealql", &["surql"]),
    ("svelte", &["svelte"]),
    ("swift", &["swift"]),
    ("tcl", &["tcl"]),
    ("template", &["hbs", "plist", "xml"]),
    (
        "terminal",
        &[
            "bash",
            "bash_aliases",
            "bash_login",
            "bash_logout",
            "bash_profile",
            "bashrc",
            "brushrc",
            "fish",
            "nu",
            "profile",
            "ps1",
            "sh",
            "zlogin",
            "zlogout",
            "zprofile",
            "zsh",
            "zsh_aliases",
            "zsh_histfile",
            "zsh_history",
            "zshenv",
            "zshrc",
        ],
    ),
    ("terraform", &["tf", "tfvars"]),
    ("toml", &["toml"]),
    ("typescript", &["cts", "mts", "ts"]),
    ("v", &["v", "vsh", "vv"]),
    (
        "vcs",
        &[
            "COMMIT_EDITMSG",
            "EDIT_DESCRIPTION",
            "MERGE_MSG",
            "NOTES_EDITMSG",
            "TAG_EDITMSG",
            "gitattributes",
            "gitignore",
            "gitkeep",
            "gitmodules",
        ],
    ),
    ("vbproj", &["vbproj"]),
    ("video", &["avi", "m4v", "mkv", "mov", "mp4", "webm", "wmv"]),
    ("vs_sln", &["sln"]),
    ("vs_suo", &["suo"]),
    ("vue", &["vue"]),
    ("vyper", &["vy", "vyi"]),
    ("wgsl", &["wgsl"]),
    ("yaml", &["yaml", "yml"]),
    ("zig", &["zig"]),
];

/// A mapping of a file type identifier to its corresponding icon.
const FILE_ICONS: &[(&str, &str)] = &[
    ("astro", "zed_file_icons/astro.svg"),
    ("audio", "zed_file_icons/audio.svg"),
    ("ballerina", "zed_file_icons/ballerina.svg"),
    ("bicep", "zed_file_icons/file.svg"),
    ("bun", "zed_file_icons/bun.svg"),
    ("c", "zed_file_icons/c.svg"),
    ("cairo", "zed_file_icons/cairo.svg"),
    ("code", "zed_file_icons/code.svg"),
    ("coffeescript", "zed_file_icons/coffeescript.svg"),
    ("cpp", "zed_file_icons/cpp.svg"),
    ("crystal", "zed_file_icons/file.svg"),
    ("csharp", "zed_file_icons/file.svg"),
    ("csproj", "zed_file_icons/file.svg"),
    ("css", "zed_file_icons/css.svg"),
    ("cue", "zed_file_icons/file.svg"),
    ("dart", "zed_file_icons/dart.svg"),
    ("default", "zed_file_icons/file.svg"),
    ("diff", "zed_file_icons/diff.svg"),
    ("docker", "zed_file_icons/docker.svg"),
    ("document", "zed_file_icons/book.svg"),
    ("editorconfig", "zed_file_icons/editorconfig.svg"),
    ("elixir", "zed_file_icons/elixir.svg"),
    ("elm", "zed_file_icons/elm.svg"),
    ("erlang", "zed_file_icons/erlang.svg"),
    ("eslint", "zed_file_icons/eslint.svg"),
    ("font", "zed_file_icons/font.svg"),
    ("fsharp", "zed_file_icons/fsharp.svg"),
    ("fsproj", "zed_file_icons/file.svg"),
    ("gitlab", "zed_file_icons/gitlab.svg"),
    ("gleam", "zed_file_icons/gleam.svg"),
    ("go", "zed_file_icons/go.svg"),
    ("graphql", "zed_file_icons/graphql.svg"),
    ("haskell", "zed_file_icons/haskell.svg"),
    ("hcl", "zed_file_icons/hcl.svg"),
    ("helm", "zed_file_icons/helm.svg"),
    ("heroku", "zed_file_icons/heroku.svg"),
    ("html", "zed_file_icons/html.svg"),
    ("image", "zed_file_icons/image.svg"),
    ("ipynb", "zed_file_icons/jupyter.svg"),
    ("java", "zed_file_icons/java.svg"),
    ("javascript", "zed_file_icons/javascript.svg"),
    ("json", "zed_file_icons/code.svg"),
    ("julia", "zed_file_icons/julia.svg"),
    ("kdl", "zed_file_icons/kdl.svg"),
    ("kotlin", "zed_file_icons/kotlin.svg"),
    ("lock", "zed_file_icons/lock.svg"),
    ("log", "zed_file_icons/info.svg"),
    ("lua", "zed_file_icons/lua.svg"),
    ("luau", "zed_file_icons/luau.svg"),
    ("markdown", "zed_file_icons/book.svg"),
    ("metal", "zed_file_icons/metal.svg"),
    ("nim", "zed_file_icons/nim.svg"),
    ("nix", "zed_file_icons/nix.svg"),
    ("ocaml", "zed_file_icons/ocaml.svg"),
    ("odin", "zed_file_icons/odin.svg"),
    ("phoenix", "zed_file_icons/phoenix.svg"),
    ("php", "zed_file_icons/php.svg"),
    ("prettier", "zed_file_icons/prettier.svg"),
    ("prisma", "zed_file_icons/prisma.svg"),
    ("puppet", "zed_file_icons/puppet.svg"),
    ("python", "zed_file_icons/python.svg"),
    ("r", "zed_file_icons/r.svg"),
    ("react", "zed_file_icons/react.svg"),
    ("roc", "zed_file_icons/roc.svg"),
    ("ruby", "zed_file_icons/ruby.svg"),
    ("rust", "zed_file_icons/rust.svg"),
    ("sass", "zed_file_icons/sass.svg"),
    ("scala", "zed_file_icons/scala.svg"),
    ("settings", "zed_file_icons/settings.svg"),
    ("solidity", "zed_file_icons/file.svg"),
    ("storage", "zed_file_icons/database.svg"),
    ("stylelint", "zed_file_icons/javascript.svg"),
    ("surrealql", "zed_file_icons/surrealql.svg"),
    ("svelte", "zed_file_icons/html.svg"),
    ("swift", "zed_file_icons/swift.svg"),
    ("tcl", "zed_file_icons/tcl.svg"),
    ("template", "zed_file_icons/html.svg"),
    ("terminal", "zed_file_icons/terminal.svg"),
    ("terraform", "zed_file_icons/terraform.svg"),
    ("toml", "zed_file_icons/toml.svg"),
    ("typescript", "zed_file_icons/typescript.svg"),
    ("v", "zed_file_icons/v.svg"),
    ("vbproj", "zed_file_icons/file.svg"),
    ("vcs", "zed_file_icons/git.svg"),
    ("video", "zed_file_icons/video.svg"),
    ("vs_sln", "zed_file_icons/file.svg"),
    ("vs_suo", "zed_file_icons/file.svg"),
    ("vue", "zed_file_icons/vue.svg"),
    ("vyper", "zed_file_icons/vyper.svg"),
    ("wgsl", "zed_file_icons/wgsl.svg"),
    ("yaml", "zed_file_icons/yaml.svg"),
    ("zig", "zed_file_icons/zig.svg"),
];

/// Resolve a path using the same filename, compound suffix and stem precedence
/// as Zed's default icon theme. Unknown associations use the generic file icon.
pub fn icon_for_path(path: &str) -> &'static str {
    fn lookup(value: &str) -> Option<&'static str> {
        let key = FILE_STEMS_BY_ICON_KEY
            .iter()
            .chain(FILE_SUFFIXES_BY_ICON_KEY)
            .find_map(|(key, values)| values.contains(&value).then_some(*key))?;
        FILE_ICONS
            .iter()
            .find_map(|(name, path)| (*name == key).then_some(*path))
    }
    if let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) {
        if let Some(icon) = lookup(name) {
            return icon;
        }
        let mut rest = name;
        let mut prefix = None;
        while let Some((head, suffix)) = rest.split_once('.') {
            prefix.get_or_insert(head);
            if let Some(icon) = lookup(suffix) {
                return icon;
            }
            rest = suffix;
        }
        if let Some(icon) = prefix.and_then(lookup) {
            return icon;
        }
    }
    "zed_file_icons/file.svg"
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AssetSource;
    #[test]
    fn resolves_filenames_suffixes_and_fallback() {
        assert_eq!(icon_for_path("src/main.rs"), "zed_file_icons/rust.svg");
        assert_eq!(icon_for_path("README.md"), "zed_file_icons/book.svg");
        assert_eq!(icon_for_path("src/app.tsx"), "zed_file_icons/react.svg");
        assert_eq!(
            icon_for_path(".github/.gitignore"),
            "zed_file_icons/git.svg"
        );
        assert_eq!(
            icon_for_path("Dockerfile.prod"),
            "zed_file_icons/docker.svg"
        );
        assert_eq!(icon_for_path("compose.yaml"), "zed_file_icons/docker.svg");
        assert_eq!(icon_for_path("unknown.unknown"), "zed_file_icons/file.svg");
    }
    #[test]
    fn all_default_icons_are_embedded() {
        for (_, path) in FILE_ICONS {
            assert!(
                crate::assets::CombinedAssets.load(path).unwrap().is_some(),
                "missing {path}"
            );
        }
    }
}
