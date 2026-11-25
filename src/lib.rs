//! Conan Center for OpenIDEA: a tool window over the recipe index conan.io
//! publishes. A search asks center2's JSON API for the recipes whose name
//! matches and lists every version of each; selecting one shows what
//! conan.io's own search API says of it (description, homepage, licences and
//! labels). Install writes the requirement into `conanfile.txt`, runs
//! `conan install`, adds the `find_package()` and `target_link_libraries()`
//! lines Conan's CMakeDeps names, and configures CMake with the generated
//! toolchain; Remove undoes it. The processes run behind handles behind a
//! cancellable task, so a long install is not cut off at the call deadline.
//! A `conanfile.py` is read, never edited.

use openidea_plugin::host::{self, Chunk, Command, Project, Task};
use openidea_plugin::views::{Body, View, ViewEvent, Viewport, Views};
use openidea_plugin::{Error, HttpRequest, Invocation, Plugin, register_plugin};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub const VIEW: &str = "conan";
const PAGE: &str = include_str!("page.html");
const CENTER: &str = "https://center2.conan.io/v2/conans/search";
const FALLBACK_CENTER: &str = "https://center.conan.io/v2/conans/search";
const DETAILS: &str = "https://conan.io/api/search/";
const BROWSE: &str = "https://conan.io/center/recipes/";
const CONANFILE: &str = "conanfile.txt";
const CONANFILE_PY: &str = "conanfile.py";
const CMAKE_LISTS: &str = "CMakeLists.txt";
/// Most recipes sent to the page for one search (all are counted).
const MAX_RECIPES: usize = 200;
/// Longest Conan/CMake output sent back (bytes).
const MAX_LOG: usize = 60_000;
/// Build folders CMake projects in the IDE use, in the order looked for.
const BUILD_DIRS: [&str; 5] = [
    "build",
    "cmake-build-debug",
    "cmake-build-release",
    "cmake-build-relwithdebinfo",
    "out/build",
];
/// CMakeDeps only writes data for the build types it installed: both are needed
/// so a Debug configure finds the same recipes and can be debugged with GDB.
const BUILD_TYPES: [&str; 2] = ["Release", "Debug"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recipe {
    pub name: String,
    pub versions: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    pub description: String,
    pub homepage: String,
    pub licenses: Vec<String>,
    pub labels: Vec<String>,
}

#[derive(Default)]
pub struct Conan {
    /// What conan.io says of each recipe, read once.
    details: BTreeMap<String, Details>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Install,
    Remove,
}

struct Ran {
    ok: bool,
    output: String,
}

struct Outcome {
    ok: bool,
    title: String,
    output: String,
    problem: Option<String>,
}

fn post(message: &Value) -> Result<(), Error> {
    host::post_view_message(VIEW, &message.to_string())
}

/// A reference's recipe and version, as `conanfile.txt` writes them.
pub fn split_ref(reference: &str) -> Option<(String, String)> {
    let reference = reference.trim();
    let (rest, user_channel) = match reference.split_once('@') {
        Some((rest, profile)) => (rest, Some(profile)),
        None => (reference, None),
    };
    if let Some(profile) = user_channel {
        let mut parts = profile.split('/');
        let user = parts.next()?;
        let channel = parts.next()?;
        if parts.next().is_some() || user.is_empty() || channel.is_empty() {
            return None;
        }
    }
    let (name, version) = rest.split_once('/')?;
    let valid = |word: &str| {
        !word.is_empty()
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
    };
    (valid(name) && valid(version)).then(|| (name.to_owned(), version.to_owned()))
}

/// The refs of a `/v2/conans/search` answer, grouped by recipe, newest version
/// first, recipes by name.
pub fn recipes_from_json(value: &Value) -> Vec<Recipe> {
    let refs = value["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|ref_| ref_.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    recipes_from_refs(refs)
}

pub fn recipes_from_refs(refs: impl IntoIterator<Item = String>) -> Vec<Recipe> {
    let mut by_name: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for reference in refs {
        let Some((name, version)) = split_ref(&reference) else {
            continue;
        };
        let entry = by_name
            .entry(name.to_ascii_lowercase())
            .or_insert_with(|| (name.clone(), Vec::new()));
        if !entry
            .1
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&version))
        {
            entry.1.push(version);
        }
    }
    by_name
        .into_values()
        .map(|(name, mut versions)| {
            versions.sort_by(|a, b| natural_cmp(b, a));
            Recipe { name, versions }
        })
        .collect()
}

/// Compares versions digit run by digit run, so 1.10.0 is newer than 1.9.0.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut left = Chunks::new(a);
    let mut right = Chunks::new(b);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some((a_digit, a_chunk)), Some((b_digit, b_chunk))) => {
                let order = if a_digit && b_digit {
                    let a_number = a_chunk.trim_start_matches('0');
                    let b_number = b_chunk.trim_start_matches('0');
                    a_number
                        .len()
                        .cmp(&b_number.len())
                        .then_with(|| a_number.cmp(b_number))
                } else if a_digit != b_digit {
                    // A version with a suffix ("1.0.0-rc1") is older than the release.
                    b_digit.cmp(&a_digit)
                } else {
                    a_chunk
                        .to_ascii_lowercase()
                        .cmp(&b_chunk.to_ascii_lowercase())
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

struct Chunks<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Chunks<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = (bool, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.text.len() {
            return None;
        }
        let rest = &self.text[self.at..];
        let digit = rest.as_bytes()[0].is_ascii_digit();
        let end = rest
            .char_indices()
            .find(|(_, c)| c.is_ascii_digit() != digit)
            .map_or(rest.len(), |(i, _)| i);
        let chunk = &rest[..end];
        self.at += end;
        Some((digit, chunk))
    }
}

/// A `conanfile.txt` with these requirements (or none), no `[settings]`
/// section (Conan 2 takes settings from the profile) and both CMake generators.
pub fn with_requires(text: &str, requires: &[String]) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut section = String::new();
    let mut insert_at = None;
    for line in text.split('\n') {
        let words = line.trim();
        if words.len() >= 2 && words.starts_with('[') && words.ends_with(']') {
            section = words[1..words.len() - 1].trim().to_ascii_lowercase();
            if section == "settings" {
                continue;
            }
            kept.push(line.to_owned());
            if section == "requires" && insert_at.is_none() {
                insert_at = Some(kept.len());
            }
            continue;
        }
        if section == "settings"
            || (section == "requires" && !words.is_empty() && !words.starts_with('#'))
        {
            continue;
        }
        kept.push(line.to_owned());
    }
    let insert_at = match insert_at {
        Some(at) => at,
        None => {
            kept.insert(0, "[requires]".into());
            1
        }
    };
    for (i, requirement) in requires
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .enumerate()
    {
        kept.insert(insert_at + i, requirement.to_owned());
    }
    ensure_generators(&kept.join("\n"))
}

fn ensure_generators(text: &str) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    let mut section = String::new();
    let mut present: Vec<String> = Vec::new();
    let mut generators = None;
    for (i, line) in lines.iter().enumerate() {
        let words = line.trim();
        if words.len() >= 2 && words.starts_with('[') && words.ends_with(']') {
            section = words[1..words.len() - 1].trim().to_ascii_lowercase();
            if section == "generators" {
                generators = Some((i, i + 1));
            }
            continue;
        }
        if section == "generators" && !words.is_empty() && !words.starts_with('#') {
            present.push(words.to_ascii_lowercase());
            if let Some((_, at)) = generators.as_mut() {
                *at = i + 1;
            }
        }
    }
    let missing: Vec<&str> = ["CMakeDeps", "CMakeToolchain"]
        .into_iter()
        .filter(|generator| {
            !present
                .iter()
                .any(|known| known == &generator.to_ascii_lowercase())
        })
        .collect();
    if missing.is_empty() {
        return lines.join("\n");
    }
    match generators {
        Some((_, at)) => {
            for (i, generator) in missing.iter().enumerate() {
                lines.insert(at + i, (*generator).to_owned());
            }
            lines.join("\n")
        }
        None => {
            let mut out = lines.join("\n").trim_end().to_owned();
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("[generators]\n");
            out.push_str(&missing.join("\n"));
            out.push('\n');
            out
        }
    }
}

fn default_conanfile() -> String {
    "[requires]\n\n[generators]\nCMakeDeps\nCMakeToolchain\n".to_owned()
}

/// The requirements a `conanfile.txt` asks for, in file order.
pub fn read_requires(text: &str) -> Vec<String> {
    let mut section = String::new();
    let mut requires = Vec::new();
    for line in text.lines() {
        let words = line.trim();
        if words.len() >= 2 && words.starts_with('[') && words.ends_with(']') {
            section = words[1..words.len() - 1].trim().to_ascii_lowercase();
            continue;
        }
        if section != "requires" || words.is_empty() || words.starts_with('#') {
            continue;
        }
        let requirement = words.split('#').next().unwrap_or(words).trim();
        if !requirement.is_empty() {
            requires.push(requirement.to_owned());
        }
    }
    requires
}

/// The requirements a `conanfile.py` asks for, read from its quoted
/// references (`requires = "zlib/1.3"`, `self.requires("fmt/12.0")`).
pub fn python_requires(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for line in text.lines() {
        let words = line.trim();
        if words.starts_with('#') {
            continue;
        }
        let lower = words.to_ascii_lowercase();
        if !lower.contains("requires") {
            continue;
        }
        let mut rest = words;
        while let Some(start) = rest.find(['"', '\'']) {
            let quote = rest.as_bytes()[start] as char;
            let after = &rest[start + 1..];
            let Some(end) = after.find(quote) else { break };
            if let Some(parts) = split_ref(&after[..end]) {
                found.push(parts);
            }
            rest = &after[end + 1..];
        }
    }
    found
}

/// A CMakeLists.txt with `find_package()` for each package and the targets
/// added to the first executable's `target_link_libraries()`, leaving what is
/// already there alone.
pub fn with_packages(text: &str, find_packages: &[String], link_targets: &[String]) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    let target = executable_name(&lines);
    let find: Vec<String> = dedup_ci(find_packages)
        .into_iter()
        .filter(|package| {
            !lines
                .iter()
                .any(|line| find_package_name(line) == Some(package.as_str()))
        })
        .collect();
    if !find.is_empty() {
        let at = lines
            .iter()
            .position(|line| executable_of(line).is_some())
            .unwrap_or(lines.len());
        let added: Vec<String> = find
            .iter()
            .map(|package| format!("find_package({package} REQUIRED)"))
            .collect();
        lines.splice(at..at, added);
    }
    if let Some(target) = target {
        let missing: Vec<&String> = link_targets
            .iter()
            .filter(|link| !lines.iter().any(|line| line.contains(link.as_str())))
            .collect();
        if !missing.is_empty() {
            let at = lines.iter().position(|line| {
                line.to_ascii_lowercase().contains(&format!(
                    "target_link_libraries({}",
                    target.to_ascii_lowercase()
                ))
            });
            let added = missing
                .iter()
                .map(|link| link.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            match at {
                Some(i) => match lines[i].rfind(')') {
                    Some(close) => lines[i].insert_str(close, &format!(" {added}")),
                    None => {
                        lines[i].push(' ');
                        lines[i].push_str(&added);
                    }
                },
                None => {
                    let all = dedup_ci(link_targets).join(" ");
                    let line = format!("target_link_libraries({target} PRIVATE {all})");
                    // Before the empty lines a trailing newline leaves, so the file keeps one.
                    let at = lines
                        .iter()
                        .rposition(|line| !line.trim().is_empty())
                        .map_or(0, |i| i + 1);
                    lines.insert(at, line);
                }
            }
        }
    }
    lines.join("\n")
}

/// A CMakeLists.txt without the `find_package()` commands of these packages
/// and the `name::target` links they provide; a link command left with only
/// keywords goes too. Edits complete command spans, so multiline commands are
/// preserved or removed whole. Returns an error for unsupported layouts that
/// cannot be edited safely.
pub fn without_packages(text: &str, names: &[String]) -> Result<String, String> {
    let prefixes: Vec<String> = names
        .iter()
        .map(|name| format!("{}::", name.to_ascii_lowercase()))
        .collect();
    let spans = cmake_command_spans(text)?;
    let mut kept = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        let Some((start, end)) = spans.iter().copied().find(|(s, _)| *s == i) else {
            let ch = text[i..].chars().next().unwrap();
            kept.push(ch);
            i += ch.len_utf8();
            continue;
        };
        let command = &text[start..end];
        if let Some(package) = find_package_name(command)
            && names.iter().any(|name| name.eq_ignore_ascii_case(package))
        {
            i = remove_command_span(&mut kept, text, end);
            continue;
        }
        if is_target_link_libraries(command) {
            match rewrite_target_link_libraries(command, &prefixes)? {
                Some(rewritten) => kept.push_str(&rewritten),
                None => {
                    i = remove_command_span(&mut kept, text, end);
                    continue;
                }
            }
            i = end;
            continue;
        }
        kept.push_str(command);
        i = end;
    }
    Ok(kept)
}

fn remove_command_span(kept: &mut String, text: &str, end: usize) -> usize {
    let line_start = kept.rfind('\n').map(|n| n + 1).unwrap_or(0);
    if kept[line_start..].chars().all(|c| c == ' ' || c == '\t') {
        kept.truncate(line_start);
        let mut i = end;
        if text.as_bytes().get(i) == Some(&b'\r') {
            i += 1;
        }
        if text.as_bytes().get(i) == Some(&b'\n') {
            i += 1;
        }
        i
    } else {
        end
    }
}

fn cmake_command_spans(text: &str) -> Result<Vec<(usize, usize)>, String> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'#' => {
                if bytes.get(i + 1) == Some(&b'[')
                    && let Some(eq) = bracket_equals(bytes, i + 1)
                {
                    i = skip_bracket(bytes, i + 1, eq);
                    continue;
                }
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                i = skip_quoted(bytes, i)?;
            }
            b'[' => {
                if let Some(eq) = bracket_equals(bytes, i) {
                    i = skip_bracket(bytes, i, eq);
                    continue;
                }
                i += 1;
            }
            b'(' => {
                return Err("unsupported CMake layout: unexpected '(' outside a command".into());
            }
            b')' => {
                return Err("unsupported CMake layout: unexpected ')' outside a command".into());
            }
            _ if is_cmake_name_start(bytes[i]) => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_cmake_name_char(bytes[i]) {
                    i += 1;
                }
                i = skip_cmake_space(bytes, i);
                if i >= bytes.len() || bytes[i] != b'(' {
                    // Not a command invocation; keep scanning.
                    i = start + 1;
                    continue;
                }
                let end = skip_command(bytes, i)?;
                spans.push((start, end));
                i = end;
            }
            _ => i += 1,
        }
    }
    Ok(spans)
}

fn skip_command(bytes: &[u8], mut i: usize) -> Result<usize, String> {
    let mut depth = 0i32;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Ok(i);
                }
                if depth < 0 {
                    return Err("unsupported CMake layout: unbalanced ')'".into());
                }
            }
            b'#' => {
                if bytes.get(i + 1) == Some(&b'[')
                    && let Some(eq) = bracket_equals(bytes, i + 1)
                {
                    i = skip_bracket(bytes, i + 1, eq);
                    continue;
                }
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => i = skip_quoted(bytes, i)?,
            b'[' => {
                if let Some(eq) = bracket_equals(bytes, i) {
                    i = skip_bracket(bytes, i, eq);
                } else {
                    i += 1;
                }
            }
            b'\\' => i = (i + 2).min(bytes.len()),
            _ => i += 1,
        }
    }
    Err("unsupported CMake layout: unterminated command".into())
}

fn skip_cmake_space(bytes: &[u8], mut i: usize) -> usize {
    loop {
        let before = i;
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'#' {
            if bytes.get(i + 1) == Some(&b'[')
                && let Some(eq) = bracket_equals(bytes, i + 1)
            {
                i = skip_bracket(bytes, i + 1, eq);
                continue;
            }
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i == before {
            return i;
        }
    }
}

fn skip_quoted(bytes: &[u8], mut i: usize) -> Result<usize, String> {
    i += 1; // opening quote
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = (i + 2).min(bytes.len()),
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err("unsupported CMake layout: unterminated quoted string".into())
}

fn bracket_equals(bytes: &[u8], open: usize) -> Option<usize> {
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let mut i = open + 1;
    let mut eq = 0usize;
    while bytes.get(i) == Some(&b'=') {
        eq += 1;
        i += 1;
    }
    (bytes.get(i) == Some(&b'[')).then_some(eq)
}

fn skip_bracket(bytes: &[u8], open: usize, eq: usize) -> usize {
    let mut i = open + 2 + eq;
    while i < bytes.len() {
        if bytes[i] == b']' {
            let mut j = i + 1;
            let mut seen = 0usize;
            while seen < eq && bytes.get(j) == Some(&b'=') {
                seen += 1;
                j += 1;
            }
            if seen == eq && bytes.get(j) == Some(&b']') {
                return j + 1;
            }
        }
        i += 1;
    }
    bytes.len()
}

fn is_cmake_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_cmake_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+')
}

fn is_target_link_libraries(command: &str) -> bool {
    command_name(command).is_some_and(|name| name.eq_ignore_ascii_case("target_link_libraries"))
}

fn command_name(command: &str) -> Option<&str> {
    let trimmed = command.trim_start();
    let name_len = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
        .count();
    if name_len == 0 {
        return None;
    }
    let name = &trimmed[..name_len];
    let rest = skip_cmake_space(trimmed.as_bytes(), name_len);
    (trimmed.as_bytes().get(rest) == Some(&b'(')).then_some(name)
}

fn rewrite_target_link_libraries(
    command: &str,
    prefixes: &[String],
) -> Result<Option<String>, String> {
    let trimmed = command.trim_start();
    let indent = &command[..command.len() - trimmed.len()];
    let Some(open_rel) = trimmed.find('(') else {
        return Err("unsupported CMake layout: malformed target_link_libraries".into());
    };
    let open = open_rel;
    let close = trimmed
        .rfind(')')
        .ok_or_else(|| "unsupported CMake layout: unterminated target_link_libraries".to_owned())?;
    let inside = &trimmed[open + 1..close];
    let after = &trimmed[close + 1..];
    if !after.trim().is_empty() {
        return Err(
            "unsupported CMake layout: trailing content after target_link_libraries".into(),
        );
    }
    let tokens = cmake_command_tokens(inside)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    let kept: Vec<String> = std::iter::once(tokens[0].clone())
        .chain(
            tokens[1..]
                .iter()
                .filter(|token| !link_token_removed(token, prefixes))
                .cloned(),
        )
        .collect();
    if kept.len() <= 1 || kept[1..].iter().all(|token| keyword(token)) {
        return Ok(None);
    }
    let multiline = inside.contains('\n');
    let rewritten = if multiline {
        format_multiline_command(indent, "target_link_libraries", &kept)
    } else {
        format!("target_link_libraries({})", kept.join(" "))
    };
    Ok(Some(rewritten))
}

fn cmake_command_tokens(inside: &str) -> Result<Vec<String>, String> {
    let bytes = inside.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'#' | b'"' | b'[' | b'(' | b')' | b'$' | b'{' | b'}' | b';' | b'\\' => {
                return Err(
                    "unsupported CMake layout: cannot safely rewrite target_link_libraries".into(),
                );
            }
            _ => {
                let start = i;
                while i < bytes.len()
                    && !matches!(
                        bytes[i],
                        b' ' | b'\t'
                            | b'\r'
                            | b'\n'
                            | b'#'
                            | b'"'
                            | b'['
                            | b'('
                            | b')'
                            | b'$'
                            | b'{'
                            | b'}'
                            | b';'
                            | b'\\'
                    )
                {
                    i += 1;
                }
                tokens.push(inside[start..i].to_owned());
            }
        }
    }
    Ok(tokens)
}

fn link_token_removed(token: &str, prefixes: &[String]) -> bool {
    let lower = token.to_ascii_lowercase();
    prefixes
        .iter()
        .any(|prefix| lower.starts_with(prefix.as_str()))
}

fn format_multiline_command(indent: &str, name: &str, tokens: &[String]) -> String {
    let mut out = format!("{name}(\n");
    for token in tokens {
        out.push_str(indent);
        out.push_str("  ");
        out.push_str(token);
        out.push('\n');
    }
    out.push_str(indent);
    out.push(')');
    out
}

fn keyword(token: &str) -> bool {
    matches!(
        token.to_ascii_uppercase().as_str(),
        "PRIVATE"
            | "PUBLIC"
            | "INTERFACE"
            | "LINK_PRIVATE"
            | "LINK_PUBLIC"
            | "LINK_INTERFACE_LIBRARIES"
            | "DEBUG"
            | "OPTIMIZED"
            | "GENERAL"
    )
}

fn find_package_name(command: &str) -> Option<&str> {
    let trimmed = command.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("find_package(") {
        let name = command_name(trimmed)?;
        if !name.eq_ignore_ascii_case("find_package") {
            return None;
        }
    }
    let open = trimmed.find('(')?;
    let rest = &trimmed[open + 1..];
    let close = rest.rfind(')').unwrap_or(rest.len());
    let inside = &rest[..close];
    let pkg = inside
        .split(|c: char| c.is_whitespace() || c == ')' || c == ',' || c == ';' || c == '#')
        .find(|s| !s.is_empty())?;
    Some(pkg)
}

fn executable_of(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if !trimmed.to_ascii_lowercase().starts_with("add_executable(") {
        return None;
    }
    let rest = trimmed["add_executable(".len()..].trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
        .collect();
    (!name.is_empty()).then_some(name)
}

fn executable_name(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|line| executable_of(line))
}

/// The `find_package()` names and `name::name` targets Conan's CMakeDeps prints.
pub fn cmake_lines(output: &str) -> (Vec<String>, Vec<String>) {
    let mut find = Vec::new();
    let mut link = Vec::new();
    let mut inside = false;
    for line in output.lines() {
        let words = line.trim();
        let lower = words.to_ascii_lowercase();
        if lower.contains("cmakedeps necessary") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some(rest) = words.strip_prefix("find_package(") {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == ')' || c == ',')
                .next()
                .unwrap_or("");
            if !name.is_empty() {
                find.push(name.to_owned());
            }
        } else if lower.starts_with("target_link_libraries(") {
            link.extend(target_tokens(words));
        } else if lower.starts_with("generator 'cmaketoolchain'") || words.starts_with(">>>") {
            inside = false;
        }
    }
    (dedup_ci(&find), dedup_ci(&link))
}

fn target_tokens(line: &str) -> Vec<String> {
    let is_name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-');
    let mut tokens = Vec::new();
    for raw in line.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-' | ':'))
        });
        let Some((namespace, name)) = token.split_once("::") else {
            continue;
        };
        if !namespace.is_empty()
            && !name.is_empty()
            && namespace.chars().all(is_name)
            && name.chars().all(is_name)
        {
            tokens.push(token.to_owned());
        }
    }
    tokens
}

fn dedup_ci(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !out.iter().any(|known| known.eq_ignore_ascii_case(item)) {
            out.push(item.clone());
        }
    }
    out
}

fn installed(project: &Project) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    // Conan prefers a conanfile.py when both are present, so read it first.
    if let Ok(text) = project.read_file(CONANFILE_PY) {
        found.extend(python_requires(&text));
    } else if let Ok(text) = project.read_file(CONANFILE) {
        found.extend(
            read_requires(&text)
                .into_iter()
                .filter_map(|reference| split_ref(&reference)),
        );
    }
    let mut seen: Vec<String> = Vec::new();
    found.retain(|(name, _)| {
        let key = name.to_ascii_lowercase();
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    found
}

fn installed_version(installed: &[(String, String)], name: &str) -> Option<String> {
    installed
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|(_, version)| version.clone())
}

fn project_name(root: &str) -> String {
    root.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(root)
        .to_owned()
}

/// The build folder to install into: one this project already configured,
/// else `build`.
fn build_dir(project: &Project) -> String {
    BUILD_DIRS
        .iter()
        .find(|dir| {
            project
                .exists(&format!("{dir}/CMakeCache.txt"))
                .unwrap_or(false)
        })
        .map_or("build", |dir| *dir)
        .to_owned()
}

fn read_center(query: &str) -> Result<Vec<Recipe>, String> {
    let query = query.trim().trim_matches('*');
    let encoded = encode(query);
    let mut last_error = String::new();
    for base in [CENTER, FALLBACK_CENTER] {
        let url = format!("{base}?q=*{encoded}*");
        match get_json(&url) {
            Ok(value) => return Ok(recipes_from_json(&value)),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn read_details(name: &str) -> Result<Details, String> {
    let value = get_json(&format!("{DETAILS}{}", encode(name)))?;
    parse_details(name, &value)
        .ok_or_else(|| format!("Conan Center describes no recipe named {name}"))
}

fn parse_details(name: &str, value: &Value) -> Option<Details> {
    let entries = value.as_object()?.values();
    let mut first = None;
    for entry in entries {
        let entry_name = entry["name"].as_str().unwrap_or("");
        if entry_name.eq_ignore_ascii_case(name) {
            return Some(entry_details(entry));
        }
        if first.is_none() {
            first = Some(entry_details(entry));
        }
    }
    first
}

fn entry_details(entry: &Value) -> Details {
    let info = &entry["info"];
    let texts = |key: &str| {
        info[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    Details {
        description: info["description"].as_str().unwrap_or("").trim().to_owned(),
        homepage: info["homepage"].as_str().unwrap_or("").trim().to_owned(),
        licenses: texts("licenses"),
        labels: texts("labels"),
    }
}

fn get_json(url: &str) -> Result<Value, String> {
    let response = host::fetch(&HttpRequest::get(url).header("accept", "application/json"))
        .map_err(|error| error.to_string())?;
    if response.status != 200 {
        return Err(format!("{url}: HTTP {}", response.status));
    }
    serde_json::from_slice(&response.body).map_err(|error| format!("{url}: {error}"))
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'*') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Longest accumulated process output kept (bytes).
const MAX_OUTPUT: usize = 2 << 20;

/// A process behind a handle, so a long run can report progress and be
/// cancelled through the task instead of being cut off at the call deadline.
trait Spawned {
    fn wait(&self, timeout_ms: u32) -> Result<Chunk, Error>;
    fn kill(&self) -> Result<(), Error>;
}

struct HostSpawned(host::Child);

impl Spawned for HostSpawned {
    fn wait(&self, timeout_ms: u32) -> Result<Chunk, Error> {
        self.0.wait(timeout_ms)
    }

    fn kill(&self) -> Result<(), Error> {
        self.0.kill()
    }
}

type Spawn<'a> = dyn Fn(&Command) -> Result<Box<dyn Spawned>, Error> + 'a;

/// Starts commands in the project's root, as `process:<program>` allows.
fn host_spawn<'a>(
    project: &'a Project,
) -> impl Fn(&Command) -> Result<Box<dyn Spawned>, Error> + 'a {
    move |cmd| {
        host::start(cmd, Some(project))
            .map(|child| Box::new(HostSpawned(child)) as Box<dyn Spawned>)
    }
}

fn command(program: &str, args: &[String]) -> Command {
    Command::new(program).args(args.iter())
}

fn needs_profile(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("conan profile detect")
        || (lower.contains("profile")
            && ["not found", "does not exist", "doesn't exist", "missing"]
                .iter()
                .any(|word| lower.contains(word)))
}

/// One program run to its end, reporting so it isn't cut off at the call
/// deadline; a cancelled task kills it.
fn run_step(task: &Task, cmd: &Command, spawn: &Spawn) -> Result<Ran, Error> {
    let command = format!("{} {}", cmd.program, cmd.args.join(" "));
    let child = spawn(cmd)?;
    let mut output = format!("$ {command}\n");
    loop {
        let chunk = child.wait(1000)?;
        output.push_str(&chunk.stdout);
        output.push_str(&chunk.stderr);
        if output.len() > MAX_OUTPUT {
            let mut cut = MAX_OUTPUT;
            while !output.is_char_boundary(cut) {
                cut -= 1;
            }
            output.truncate(cut);
            output.push_str("\n… (output truncated)");
        }
        task.report(0, None, Some(&command));
        if task.is_cancelled() {
            let _ = child.kill();
            return Err(Error::Cancelled);
        }
        if let Some(status) = chunk.status {
            return Ok(Ran {
                ok: status == 0,
                output,
            });
        }
    }
}

fn conan_install(task: &Task, spawn: &Spawn, build: &str, may_build: bool) -> Result<Ran, Error> {
    let folder = format!("{build}/conan");
    let building = if may_build {
        "--build=missing"
    } else {
        "--build=never"
    };
    let mut output = String::new();
    for build_type in BUILD_TYPES {
        let setting = format!("build_type={build_type}");
        let args: Vec<String> = [
            "install",
            ".",
            "--output-folder",
            &folder,
            building,
            "-s",
            &setting,
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        let mut ran = run_step(task, &command("conan", &args), spawn)?;
        if !ran.ok && needs_profile(&ran.output) {
            let detect = run_step(
                task,
                &command(
                    "conan",
                    &["profile".into(), "detect".into(), "--force".into()],
                ),
                spawn,
            )?;
            ran.output.push_str(&format!("\n\n{}", detect.output));
            if detect.ok {
                let again = run_step(task, &command("conan", &args), spawn)?;
                ran.output.push_str(&format!("\n\n{}", again.output));
                ran.ok = again.ok;
            }
        }
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(&ran.output);
        if !ran.ok {
            return Ok(Ran { ok: false, output });
        }
    }
    Ok(Ran { ok: true, output })
}

/// The build type of an already configured build dir, else the IDE's Debug
/// default: a new project gets symbols, so GDB can bind breakpoints.
fn build_type_of(cache: Option<&str>) -> String {
    cache
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("CMAKE_BUILD_TYPE:STRING="))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "Debug".into())
}

fn configure(project: &Project, task: &Task, spawn: &Spawn, build: &str) -> Result<Ran, Error> {
    let conan = format!("{build}/conan");
    let mut args: Vec<String> = Vec::new();
    let cache = project.read_file(&format!("{build}/CMakeCache.txt")).ok();
    if let Some(cache) = &cache {
        // A cache made without Conan's toolchain would not find the packages.
        if !cache.to_ascii_lowercase().contains("conan_toolchain.cmake") {
            args.push("--fresh".into());
        }
    }
    let build_type = build_type_of(cache.as_deref());
    args.extend(
        [
            "-S",
            ".",
            "-B",
            build,
            &format!("-DCMAKE_TOOLCHAIN_FILE={conan}/conan_toolchain.cmake"),
            &format!("-DCMAKE_PREFIX_PATH={conan}"),
            &format!("-DCMAKE_BUILD_TYPE={build_type}"),
            "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
        ]
        .iter()
        .map(|arg| (*arg).to_owned()),
    );
    let first = run_step(task, &command("cmake", &args), spawn)?;
    let mut output = first.output;
    if !first.ok
        && output.contains("--fresh")
        && output.to_ascii_lowercase().contains("unknown argument")
    {
        // CMake before 3.24 has no --fresh; the cached values stay, as before.
        args.retain(|arg| arg != "--fresh");
        let again = run_step(task, &command("cmake", &args), spawn)?;
        output.push_str(&format!("\n\n{}", again.output));
        return Ok(Ran {
            ok: again.ok,
            output,
        });
    }
    Ok(Ran {
        ok: first.ok,
        output,
    })
}

fn apply_change(
    project: &Project,
    task: &Task,
    spawn: &Spawn,
    change: Change,
    name: &str,
    version: Option<&str>,
) -> Outcome {
    let title = match change {
        Change::Install => match version {
            Some(version) => format!("Install {name}/{version}"),
            None => format!("Install {name}"),
        },
        Change::Remove => format!("Remove {name}"),
    };
    match try_change(project, task, spawn, change, name, version, &title) {
        Ok(outcome) => outcome,
        Err(problem) => Outcome {
            ok: false,
            title,
            output: String::new(),
            problem: Some(problem),
        },
    }
}

fn try_change(
    project: &Project,
    task: &Task,
    spawn: &Spawn,
    change: Change,
    name: &str,
    version: Option<&str>,
    title: &str,
) -> Result<Outcome, String> {
    let error = |error: Error| error.to_string();
    let has_txt = project.exists(CONANFILE).map_err(error)?;
    let has_py = project.exists(CONANFILE_PY).map_err(error)?;
    if has_py {
        return Err(
            "the project has a conanfile.py; edit it by hand, this plugin only changes conanfile.txt".to_owned()
        );
    }
    let before = if has_txt {
        project.read_file(CONANFILE).map_err(error)?
    } else {
        default_conanfile()
    };
    let has_lists = project.exists(CMAKE_LISTS).map_err(error)?;
    let build = build_dir(project);
    // What the requirements contribute to CMake now, so Remove can take away a
    // recipe whose package name differs from its recipe name (zlib → ZLIB).
    // Best effort: an install that fails here only leaves the name-based removal.
    let before_find = if change == Change::Remove && has_txt && has_lists {
        // Only reporting what is installed, so never build here.
        conan_install(task, spawn, &build, false)
            .map_or_else(|_| Vec::new(), |ran| cmake_lines(&ran.output).0)
    } else {
        Vec::new()
    };
    let mut requires = read_requires(&before);
    let needed = match change {
        Change::Install => {
            let version = version.ok_or_else(|| format!("choose a version of {name}"))?;
            if split_ref(&format!("{name}/{version}")).is_none() {
                return Err(format!("{name}/{version} is not a valid Conan reference"));
            }
            requires.retain(|known| {
                split_ref(known).is_none_or(|(other, _)| !other.eq_ignore_ascii_case(name))
            });
            requires.push(format!("{name}/{version}"));
            requires.clone()
        }
        Change::Remove => {
            requires.retain(|known| {
                split_ref(known).is_none_or(|(other, _)| !other.eq_ignore_ascii_case(name))
            });
            if requires.len() == read_requires(&before).len() {
                return Err(format!("the project does not ask for {name}"));
            }
            requires.clone()
        }
    };
    project
        .write_file(CONANFILE, &with_requires(&before, &requires))
        .map_err(error)?;

    let installed = conan_install(task, spawn, &build, true).map_err(error)?;
    if !installed.ok {
        return Ok(Outcome {
            ok: false,
            title: title.to_owned(),
            output: installed.output,
            problem: Some("conan install failed; its output says why".into()),
        });
    }
    let (mut find, link) = cmake_lines(&installed.output);
    if find.is_empty() {
        // Conan's own words are the best source; its CMakeDeps block is always
        // printed, but a recipe's package name may differ from its recipe name.
        find = needed
            .iter()
            .filter_map(|reference| split_ref(reference).map(|(name, _)| name))
            .collect();
    }
    if has_lists {
        let lists = project.read_file(CMAKE_LISTS).map_err(error)?;
        let mut edited = lists.clone();
        if change == Change::Remove {
            let mut removed = vec![name.to_owned()];
            removed.extend(
                before_find
                    .iter()
                    .filter(|package| !find.iter().any(|kept| kept.eq_ignore_ascii_case(package)))
                    .cloned(),
            );
            edited = without_packages(&edited, &removed)?;
        }
        if !requires.is_empty() {
            edited = with_packages(&edited, &find, &link);
        }
        if edited != lists {
            project.write_file(CMAKE_LISTS, &edited).map_err(error)?;
        }
        let configured = configure(project, task, spawn, &build).map_err(error)?;
        let output = format!("{}\n\n{}", installed.output, configured.output);
        return Ok(Outcome {
            ok: configured.ok,
            title: title.to_owned(),
            output,
            problem: (!configured.ok).then(|| {
                "installed, but the CMake configure failed; its output says why".to_owned()
            }),
        });
    }
    Ok(Outcome {
        ok: true,
        title: title.to_owned(),
        output: installed.output,
        problem: None,
    })
}

fn limit_log(text: &str) -> String {
    if text.len() <= MAX_LOG {
        return text.to_owned();
    }
    let mut cut = MAX_LOG;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… (output truncated)", &text[..cut])
}

fn search_message(project: Option<&Project>, seq: u64, query: &str) -> Value {
    let Some(project) = project else {
        return json!({
            "op": "search",
            "seq": seq,
            "query": query,
            "recipes": [],
            "total": 0,
            "error": "open a folder first",
        });
    };
    let installed = installed(project);
    let recipes = if query.is_empty() {
        let refs = installed
            .iter()
            .map(|(name, version)| format!("{name}/{version}"))
            .collect::<Vec<_>>();
        recipes_from_refs(refs)
    } else {
        match read_center(query) {
            Ok(mut recipes) => {
                sort_for(&mut recipes, query);
                recipes
            }
            Err(error) => {
                return json!({
                    "op": "search",
                    "seq": seq,
                    "query": query,
                    "recipes": [],
                    "total": 0,
                    "error": error,
                });
            }
        }
    };
    let total = recipes.len();
    let shown: Vec<Value> = recipes
        .iter()
        .take(MAX_RECIPES)
        .map(|recipe| {
            json!({
                "name": recipe.name,
                "versions": recipe.versions,
                "installed": installed_version(&installed, &recipe.name),
            })
        })
        .collect();
    json!({ "op": "search", "seq": seq, "query": query, "recipes": shown, "total": total, "error": null })
}

fn sort_for(recipes: &mut [Recipe], query: &str) {
    let wanted = query.to_ascii_lowercase();
    let rank = |name: &str| {
        let name = name.to_ascii_lowercase();
        if name == wanted {
            0
        } else if name.starts_with(&wanted) {
            1
        } else if name.contains(&wanted) {
            2
        } else {
            3
        }
    };
    recipes.sort_by(|a, b| {
        rank(&a.name).cmp(&rank(&b.name)).then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        })
    });
}

impl Plugin for Conan {
    fn run_command(
        &mut self,
        id: &str,
        _ctx: &Invocation,
        _arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        match id {
            "open" => {
                host::reveal_view(VIEW)?;
                Ok(None)
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }
}

impl Views for Conan {
    fn render(
        &mut self,
        view: &str,
        _viewport: Option<Viewport>,
        ctx: &Invocation,
    ) -> Result<View, Error> {
        if view != VIEW {
            return Err(Error::NotFound(view.into()));
        }
        let start = match ctx.project() {
            Some(project) => {
                let root = project.root().unwrap_or_default();
                let recipe = if project.exists(CONANFILE_PY).unwrap_or(false) {
                    json!(CONANFILE_PY)
                } else if project.exists(CONANFILE).unwrap_or(false) {
                    json!(CONANFILE)
                } else {
                    Value::Null
                };
                let installed: Vec<Value> = installed(&project)
                    .into_iter()
                    .map(|(name, version)| json!({ "name": name, "version": version }))
                    .collect();
                json!({
                    "project": {
                        "name": project_name(&root),
                        "root": root,
                        "recipe": recipe,
                        "cmakeLists": project.exists(CMAKE_LISTS).unwrap_or(false),
                    },
                    "installed": installed,
                })
            }
            None => json!({ "project": null, "installed": [] }),
        }
        .to_string()
        .replace('<', "\\u003c");
        let page = PAGE.replacen(r#""__START__""#, &start, 1);
        Ok(View::new(Body::Html(page)))
    }

    fn handle(&mut self, view: &str, event: ViewEvent, ctx: &Invocation) -> Result<(), Error> {
        if view != VIEW {
            return Err(Error::NotFound(view.into()));
        }
        let ViewEvent::Message(text) = event else {
            return Ok(());
        };
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            return Ok(());
        };
        match message["op"].as_str().unwrap_or_default() {
            "search" => {
                let seq = message["seq"].as_u64().unwrap_or(0);
                let query = message["query"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                post(&search_message(ctx.project().as_ref(), seq, &query))
            }
            "details" => {
                let name = message["name"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if name.is_empty() {
                    return Ok(());
                }
                let key = name.to_ascii_lowercase();
                if let Some(details) = self.details.get(&key) {
                    return post(&details_message(&name, details));
                }
                match read_details(&name) {
                    Ok(details) => {
                        self.details.insert(key, details.clone());
                        post(&details_message(&name, &details))
                    }
                    Err(error) => post(&json!({ "op": "details", "name": name, "error": error })),
                }
            }
            "install" | "remove" => {
                let change = if message["op"] == "install" {
                    Change::Install
                } else {
                    Change::Remove
                };
                let name = message["name"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let version = message["version"].as_str().map(str::to_owned);
                if name.is_empty() {
                    return Ok(());
                }
                let Some(project) = ctx.project() else {
                    return post(&json!({
                        "op": "result",
                        "ok": false,
                        "name": name,
                        "title": "Conan",
                        "output": "",
                        "problem": "open a folder first",
                    }));
                };
                let title = match (change, &version) {
                    (Change::Install, Some(version)) => format!("Install {name}/{version}"),
                    (Change::Install, None) => format!("Install {name}"),
                    (Change::Remove, _) => format!("Remove {name}"),
                };
                post(&json!({ "op": "busy", "busy": true, "title": title }))?;
                let task = host::begin_task(&format!("Conan: {title}"), true);
                let outcome = apply_change(
                    &project,
                    &task,
                    &host_spawn(&project),
                    change,
                    &name,
                    version.as_deref(),
                );
                post(&json!({
                    "op": "result",
                    "ok": outcome.ok,
                    "name": name,
                    "title": outcome.title,
                    "output": limit_log(&outcome.output),
                    "problem": outcome.problem,
                }))
            }
            "open" => {
                let name = message["name"].as_str().unwrap_or_default().trim();
                if name.is_empty() {
                    return Ok(());
                }
                host::open_url(&format!("{BROWSE}{}", encode(name)))
            }
            "open-url" => {
                let url = message["url"].as_str().unwrap_or_default();
                if url.starts_with("https://") || url.starts_with("http://") {
                    host::open_url(url)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn details_message(name: &str, details: &Details) -> Value {
    json!({
        "op": "details",
        "name": name,
        "description": details.description,
        "homepage": details.homepage,
        "licenses": details.licenses,
        "labels": details.labels,
        "error": null,
    })
}

register_plugin!(Conan, views);

#[cfg(test)]
mod tests {
    use super::*;
    use openidea_plugin::mock::MockHost;
    use openidea_plugin::{HttpRequest, HttpResponse};

    const FMT_AND_ZLIB: &str = "\
Generator 'CMakeDeps' calling 'generate()'
CMakeDeps necessary find_package() and targets for your CMakeLists.txt
    find_package(fmt)
    find_package(ZLIB)
    target_link_libraries(... fmt::fmt ZLIB::ZLIB)
Generator 'CMakeToolchain' calling 'generate()'
CMakeToolchain generated: conan_toolchain.cmake
Install finished successfully
";

    const ZLIB_ONLY: &str = "\
CMakeDeps necessary find_package() and targets for your CMakeLists.txt
    find_package(ZLIB)
    target_link_libraries(... ZLIB::ZLIB)
Generator 'CMakeToolchain' calling 'generate()'
";

    fn replies(host: &MockHost) -> Vec<Value> {
        host.view_messages()
            .into_iter()
            .filter(|(view, _)| view == VIEW)
            .map(|(_, message)| serde_json::from_str(&message).expect("JSON from the plugin"))
            .collect()
    }

    fn last(host: &MockHost) -> Value {
        replies(host).pop().expect("a message for the view")
    }

    fn response(json: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: json.as_bytes().to_vec(),
        }
    }

    /// A process that answers `wait` with one scripted chunk, as a real one
    /// would when its output arrives all at once.
    struct FakeChild {
        chunk: std::cell::RefCell<Option<Chunk>>,
    }

    impl FakeChild {
        fn done(stdout: &str) -> FakeChild {
            FakeChild {
                chunk: std::cell::RefCell::new(Some(Chunk {
                    stdout: stdout.into(),
                    stderr: String::new(),
                    status: Some(0),
                })),
            }
        }
    }

    impl Spawned for FakeChild {
        fn wait(&self, _timeout_ms: u32) -> Result<Chunk, Error> {
            Ok(self.chunk.borrow_mut().take().unwrap_or(Chunk {
                stdout: String::new(),
                stderr: String::new(),
                status: Some(0),
            }))
        }

        fn kill(&self) -> Result<(), Error> {
            Ok(())
        }
    }

    fn invocation(host: &MockHost) -> Invocation<'static> {
        host.invocation()
    }

    #[test]
    fn refs_are_grouped_and_versions_sorted_naturally() {
        let value: Value = serde_json::from_str(
            r#"{"results":["fmt/10.2.1@_/_","fmt/11.0.2@_/_","zlib/1.3.1@_/_","fmt/9.1.0@_/_","zlib/1.2.13@_/_"]}"#,
        )
        .unwrap();
        let recipes = recipes_from_json(&value);
        assert_eq!(
            recipes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["fmt", "zlib"]
        );
        assert_eq!(recipes[0].versions, ["11.0.2", "10.2.1", "9.1.0"]);
        assert_eq!(recipes[1].versions, ["1.3.1", "1.2.13"]);
        assert_eq!(
            split_ref("zlib/1.3.1@user/stable"),
            Some(("zlib".into(), "1.3.1".into()))
        );
        assert_eq!(split_ref("zlib"), None);
    }

    #[test]
    fn recipe_keeps_other_sections_and_drops_settings() {
        let before = "[requires]\nzlib/1.3.1\n\n[settings]\ncompiler=gcc\n\n[generators]\nPkgConfigDeps\n\n[options]\nzlib:shared=True\n";
        let after = with_requires(before, &["fmt/12.2.0".into(), "zlib/1.4.0".into()]);
        assert!(!after.contains("[settings]"));
        assert!(!after.contains("compiler=gcc"));
        assert!(
            after.contains("[requires]\nfmt/12.2.0\nzlib/1.4.0\n"),
            "{after}"
        );
        assert!(
            after.contains("[generators]\nPkgConfigDeps\nCMakeDeps\nCMakeToolchain\n"),
            "{after}"
        );
        assert!(after.contains("[options]\nzlib:shared=True"), "{after}");
        assert_eq!(read_requires(&after), ["fmt/12.2.0", "zlib/1.4.0"]);

        let fresh = with_requires("", &["fmt/12.2.0".into()]);
        assert_eq!(
            fresh,
            "[requires]\nfmt/12.2.0\n\n[generators]\nCMakeDeps\nCMakeToolchain\n"
        );
    }

    #[test]
    fn cmake_edits_are_idempotent_and_removable() {
        let lists = "cmake_minimum_required(VERSION 3.20)\nproject(app LANGUAGES CXX)\n\nadd_executable(app src/main.cpp)\n";
        let with = with_packages(
            lists,
            &["fmt".into(), "ZLIB".into()],
            &["fmt::fmt".into(), "ZLIB::ZLIB".into()],
        );
        assert!(
            with.contains(
                "find_package(fmt REQUIRED)\nfind_package(ZLIB REQUIRED)\nadd_executable"
            ),
            "{with}"
        );
        assert!(
            with.ends_with("target_link_libraries(app PRIVATE fmt::fmt ZLIB::ZLIB)\n"),
            "{with}"
        );
        assert_eq!(
            with_packages(&with, &["fmt".into()], &["fmt::fmt".into()]),
            with
        );

        let without = without_packages(&with, &["fmt".to_owned()]).unwrap();
        assert!(!without.contains("find_package(fmt"), "{without}");
        assert!(without.contains("find_package(ZLIB REQUIRED)"), "{without}");
        assert!(
            without.contains("target_link_libraries(app PRIVATE ZLIB::ZLIB)"),
            "{without}"
        );
        assert_eq!(
            without_packages(
                &without_packages(&with, &["fmt".to_owned()]).unwrap(),
                &["fmt".to_owned()]
            )
            .unwrap(),
            without_packages(&with, &["fmt".to_owned()]).unwrap()
        );

        let only = "add_executable(app main.cpp)\ntarget_link_libraries(app PRIVATE fmt::fmt)\n";
        assert!(
            !without_packages(only, &["fmt".to_owned()])
                .unwrap()
                .contains("target_link_libraries"),
            "{only}"
        );
    }

    #[test]
    fn cmake_multiline_commands_are_removed_as_complete_spans() {
        let lists = "cmake_minimum_required(VERSION 3.20)\n\
project(app LANGUAGES CXX)\n\
\n\
find_package(fmt\n\
  CONFIG REQUIRED\n\
)\n\
find_package(ZLIB REQUIRED)\n\
add_executable(app src/main.cpp)\n\
target_link_libraries(app\n\
  PRIVATE\n\
  fmt::fmt\n\
  ZLIB::ZLIB\n\
)\n";
        let without = without_packages(lists, &["fmt".to_owned()]).unwrap();
        assert!(!without.contains("CONFIG REQUIRED"), "{without}");
        assert!(!without.contains("fmt::fmt"), "{without}");
        assert!(!without.contains("find_package(fmt"), "{without}");
        assert!(without.contains("find_package(ZLIB REQUIRED)"), "{without}");
        assert!(without.contains("ZLIB::ZLIB"), "{without}");
        assert!(without.contains("target_link_libraries("), "{without}");
        assert_eq!(
            without.matches(')').count(),
            lists.matches(')').count() - 1,
            "{without}"
        );
    }

    #[test]
    fn unsupported_cmake_layout_is_rejected_before_writing() {
        let lists = "add_executable(app main.cpp)\n\
target_link_libraries(app\n\
  PRIVATE\n\
  fmt::fmt\n\
  $<IF:$<CONFIG:Debug>,ZLIB::ZLIBDEBUG,ZLIB::ZLIB>\n\
)\n";
        let err = without_packages(lists, &["fmt".to_owned()]).unwrap_err();
        assert!(err.contains("unsupported CMake layout"), "{err}");

        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "project.write",
            "process:conan",
            "process:cmake",
        ]);
        host.add_file(
            "conanfile.txt",
            "[requires]\nfmt/11.0.2\nzlib/1.3.1\n\n[generators]\nCMakeDeps\nCMakeToolchain\n",
        );
        host.add_file("CMakeLists.txt", lists);
        let spawn = move |cmd: &Command| -> Result<Box<dyn Spawned>, Error> {
            match cmd.program.as_str() {
                "conan" => Ok(Box::new(FakeChild::done(FMT_AND_ZLIB))),
                "cmake" => Ok(Box::new(FakeChild::done(""))),
                other => panic!("unexpected program {other}"),
            }
        };
        let project = invocation(&host).project().expect("project");
        let task = host::begin_task("test", true);
        let outcome = apply_change(&project, &task, &spawn, Change::Remove, "fmt", None);
        assert!(!outcome.ok, "{:?}", outcome.problem);
        let problem = outcome.problem.unwrap_or_default();
        assert!(problem.contains("unsupported CMake layout"), "{problem}");
        let after = project.read_file("CMakeLists.txt").unwrap();
        assert_eq!(after, lists, "CMakeLists must not be damaged");
    }

    #[test]
    fn removing_through_apply_change_preserves_multiline_cmake_commands() {
        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "project.write",
            "process:conan",
            "process:cmake",
        ]);
        host.add_file(
            "conanfile.txt",
            "[requires]\nfmt/11.0.2\nzlib/1.3.1\n\n[generators]\nCMakeDeps\nCMakeToolchain\n",
        );
        host.add_file(
            "CMakeLists.txt",
            "cmake_minimum_required(VERSION 3.20)\n\
project(app LANGUAGES CXX)\n\
add_executable(app src/main.cpp)\n\
find_package(fmt\n\
  CONFIG REQUIRED\n\
)\n\
find_package(ZLIB REQUIRED)\n\
target_link_libraries(app\n\
  PRIVATE\n\
  fmt::fmt\n\
  ZLIB::ZLIB\n\
)\n",
        );
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let spawn_calls = calls.clone();
        let spawn = move |cmd: &Command| -> Result<Box<dyn Spawned>, Error> {
            match cmd.program.as_str() {
                "conan" => {
                    spawn_calls.set(spawn_calls.get() + 1);
                    let before = cmd.args.contains(&"--build=never".to_string());
                    let output = if before {
                        FMT_AND_ZLIB
                    } else {
                        "CMakeDeps necessary find_package() and targets for your CMakeLists.txt\n    find_package(ZLIB)\n    target_link_libraries(... ZLIB::ZLIB)\n"
                    };
                    Ok(Box::new(FakeChild::done(output)))
                }
                "cmake" => Ok(Box::new(FakeChild::done(""))),
                other => panic!("unexpected program {other}"),
            }
        };
        let project = invocation(&host).project().expect("project");
        let task = host::begin_task("test", true);
        let outcome = apply_change(&project, &task, &spawn, Change::Remove, "fmt", None);
        assert!(outcome.ok, "{:?}", outcome.problem);
        let lists = project.read_file("CMakeLists.txt").unwrap();
        assert!(!lists.contains("find_package(fmt"), "{lists}");
        assert!(!lists.contains("CONFIG REQUIRED"), "{lists}");
        assert!(!lists.contains("fmt::fmt"), "{lists}");
        assert!(lists.contains("find_package(ZLIB REQUIRED)"), "{lists}");
        assert!(lists.contains("ZLIB::ZLIB"), "{lists}");
        assert_eq!(
            lists.matches("target_link_libraries(").count(),
            1,
            "{lists}"
        );
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn removing_a_recipe_takes_away_its_differently_named_cmake_package() {
        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "project.write",
            "process:conan",
            "process:cmake",
        ]);
        host.add_file(
            "conanfile.txt",
            "[requires]\nzlib/1.3.1\n\n[generators]\nCMakeDeps\nCMakeToolchain\n",
        );
        host.add_file(
            "CMakeLists.txt",
            "add_executable(app main.cpp)\nfind_package(ZLIB REQUIRED)\ntarget_link_libraries(app PRIVATE ZLIB::ZLIB)\n",
        );
        // The installs before the edit still list ZLIB; the ones after it, with
        // no requirements left, list nothing. Each install now runs twice.
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let spawn_calls = calls.clone();
        let spawn = move |cmd: &Command| -> Result<Box<dyn Spawned>, Error> {
            match cmd.program.as_str() {
                "conan" => {
                    spawn_calls.set(spawn_calls.get() + 1);
                    let before = cmd.args.contains(&"--build=never".to_string());
                    Ok(Box::new(FakeChild::done(if before {
                        ZLIB_ONLY
                    } else {
                        "Install finished successfully\n"
                    })))
                }
                "cmake" => Ok(Box::new(FakeChild::done(""))),
                other => panic!("unexpected program {other}"),
            }
        };
        let project = invocation(&host).project().expect("project");
        let task = host::begin_task("test", true);
        let outcome = apply_change(&project, &task, &spawn, Change::Remove, "zlib", None);
        assert!(outcome.ok, "{:?}", outcome.problem);
        let lists = project.read_file("CMakeLists.txt").unwrap();
        assert!(!lists.contains("ZLIB"), "{lists}");
        assert!(!lists.contains("find_package"), "{lists}");
        assert_eq!(
            calls.get(),
            4,
            "the before and after installs, each for two build types"
        );
    }

    #[test]
    fn cmake_deps_names_are_read_from_conans_words() {
        let (find, link) = cmake_lines(FMT_AND_ZLIB);
        assert_eq!(find, ["fmt", "ZLIB"]);
        assert_eq!(link, ["fmt::fmt", "ZLIB::ZLIB"]);
    }

    #[test]
    fn a_search_lists_versions_and_the_installed_one() {
        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "net:center2.conan.io",
            "net:center.conan.io",
        ]);
        host.add_file("conanfile.txt", "[requires]\nfmt/12.1.0\n");
        host.on_fetch(|request: &HttpRequest| {
            assert!(
                request
                    .url
                    .starts_with("https://center2.conan.io/v2/conans/search?q=*fmt*"),
                "unexpected URL {}",
                request.url
            );
            Ok(response(
                r#"{"results":["fmt/12.2.0@_/_","fmt/11.0.2@_/_","zlib/1.3.1@_/_"]}"#,
            ))
        });
        let mut conan = Conan::default();
        conan
            .handle(
                VIEW,
                ViewEvent::Message(json!({ "op": "search", "seq": 7, "query": "fmt" }).to_string()),
                &invocation(&host),
            )
            .unwrap();
        let message = last(&host);
        assert_eq!(message["seq"], 7);
        assert_eq!(message["total"], 2);
        assert_eq!(message["recipes"][0]["name"], "fmt");
        assert_eq!(
            message["recipes"][0]["versions"],
            json!(["12.2.0", "11.0.2"])
        );
        assert_eq!(message["recipes"][0]["installed"], "12.1.0");
        assert_eq!(message["recipes"][1]["name"], "zlib");

        // An empty query lists the project's own packages, with no network.
        conan
            .handle(
                VIEW,
                ViewEvent::Message(json!({ "op": "search", "seq": 8, "query": "" }).to_string()),
                &invocation(&host),
            )
            .unwrap();
        let message = last(&host);
        assert_eq!(message["recipes"].as_array().unwrap().len(), 1);
        assert_eq!(message["recipes"][0]["name"], "fmt");
        assert_eq!(host.requests().len(), 1);
    }

    #[test]
    fn details_come_from_conan_ios_search_api() {
        let host = MockHost::install().with_capabilities(&["project.read", "net:conan.io"]);
        host.on_fetch(|request: &HttpRequest| {
            assert_eq!(request.url, "https://conan.io/api/search/fmt");
            Ok(response(
                r#"{"0":{"name":"fmt","info":{"version":"12.2.0","description":"A fast format library.","homepage":"https://fmt.dev","licenses":["MIT"],"labels":["format","printf"]}}}"#,
            ))
        });
        let mut conan = Conan::default();
        conan
            .handle(
                VIEW,
                ViewEvent::Message(json!({ "op": "details", "name": "fmt" }).to_string()),
                &invocation(&host),
            )
            .unwrap();
        let message = last(&host);
        assert_eq!(message["description"], "A fast format library.");
        assert_eq!(message["homepage"], "https://fmt.dev");
        assert_eq!(message["licenses"], json!(["MIT"]));
        assert_eq!(message["labels"], json!(["format", "printf"]));

        // Read once: the second request is answered from memory.
        conan
            .handle(
                VIEW,
                ViewEvent::Message(json!({ "op": "details", "name": "FMT" }).to_string()),
                &invocation(&host),
            )
            .unwrap();
        assert_eq!(host.requests().len(), 1);
    }

    #[test]
    fn install_writes_the_recipe_and_cmake_then_remove_undoes_it() {
        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "project.write",
            "process:conan",
            "process:cmake",
        ]);
        host.add_file(
            "conanfile.txt",
            "[requires]\nzlib/1.3.1\n\n[generators]\nCMakeDeps\nCMakeToolchain\n",
        );
        host.add_file(
            "CMakeLists.txt",
            "cmake_minimum_required(VERSION 3.20)\nproject(app LANGUAGES CXX)\n\nadd_executable(app src/main.cpp)\n",
        );
        // conan runs twice per install (Release + Debug): the fmt install, then
        // the remove's before and after installs.
        let conan_calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::<Vec<String>>::new()));
        let recorded = conan_calls.clone();
        let spawn = move |cmd: &Command| -> Result<Box<dyn Spawned>, Error> {
            match cmd.program.as_str() {
                "conan" => {
                    let mut calls = recorded.borrow_mut();
                    calls.push(cmd.args.clone());
                    let n = calls.len();
                    drop(calls);
                    assert_eq!(cmd.args[0], "install");
                    assert!(cmd.args.contains(&"--output-folder".to_string()));
                    // 1-2: install, 3-4: remove's before, 5-6: remove's after
                    // (fmt is gone from the recipe by then).
                    Ok(Box::new(FakeChild::done(if n > 4 {
                        ZLIB_ONLY
                    } else {
                        FMT_AND_ZLIB
                    })))
                }
                "cmake" => {
                    assert!(cmd.args.contains(
                        &"-DCMAKE_TOOLCHAIN_FILE=build/conan/conan_toolchain.cmake".to_string()
                    ));
                    assert!(
                        cmd.args
                            .contains(&"-DCMAKE_PREFIX_PATH=build/conan".to_string())
                    );
                    // No CMakeCache.txt in the mock: a new project starts in Debug.
                    assert!(
                        cmd.args.contains(&"-DCMAKE_BUILD_TYPE=Debug".to_string()),
                        "{:?}",
                        cmd.args
                    );
                    Ok(Box::new(FakeChild::done(
                        "-- Configuring done\n-- Generating done\n",
                    )))
                }
                other => panic!("unexpected program {other}"),
            }
        };
        let project = invocation(&host).project().expect("project");
        let task = host::begin_task("test", true);

        let outcome = apply_change(
            &project,
            &task,
            &spawn,
            Change::Install,
            "fmt",
            Some("12.2.0"),
        );
        assert!(outcome.ok, "{:?}", outcome.problem);
        let recipe = project.read_file("conanfile.txt").unwrap();
        assert!(
            recipe.contains("zlib/1.3.1") && recipe.contains("fmt/12.2.0"),
            "{recipe}"
        );
        let lists = project.read_file("CMakeLists.txt").unwrap();
        assert!(lists.contains("find_package(fmt REQUIRED)"), "{lists}");
        assert!(lists.contains("find_package(ZLIB REQUIRED)"), "{lists}");
        assert!(
            lists.contains("target_link_libraries(app PRIVATE fmt::fmt ZLIB::ZLIB)"),
            "{lists}"
        );

        let outcome = apply_change(&project, &task, &spawn, Change::Remove, "fmt", None);
        assert!(outcome.ok, "{:?}", outcome.problem);
        let recipe = project.read_file("conanfile.txt").unwrap();
        assert!(!recipe.contains("fmt/"), "{recipe}");
        let lists = project.read_file("CMakeLists.txt").unwrap();
        assert!(!lists.contains("find_package(fmt"), "{lists}");
        assert!(!lists.contains("fmt::fmt"), "{lists}");
        assert!(lists.contains("find_package(ZLIB REQUIRED)"), "{lists}");
        assert!(
            lists.contains("target_link_libraries(app PRIVATE ZLIB::ZLIB)"),
            "{lists}"
        );
        assert_eq!(
            conan_calls.borrow().len(),
            6,
            "install, before and after, each for two build types"
        );
        let settings = conan_calls
            .borrow()
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            settings.contains("build_type=Release") && settings.contains("build_type=Debug"),
            "{settings}"
        );
    }

    #[test]
    fn configure_keeps_the_cached_build_type() {
        assert_eq!(build_type_of(None), "Debug");
        assert_eq!(
            build_type_of(Some("CMAKE_BUILD_TYPE:STRING=Release\n")),
            "Release"
        );
        assert_eq!(
            build_type_of(Some("CMAKE_BUILD_TYPE:STRING=RelWithDebInfo\n")),
            "RelWithDebInfo"
        );
        assert_eq!(build_type_of(Some("CMAKE_BUILD_TYPE:STRING=\n")), "Debug");
    }

    #[test]
    fn a_cached_release_configure_stays_release() {
        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "project.write",
            "process:conan",
            "process:cmake",
        ]);
        host.add_file("conanfile.txt", "[requires]\nzlib/1.3.1\n");
        host.add_file("CMakeLists.txt", "add_executable(app main.cpp)\n");
        host.add_file(
            "build/CMakeCache.txt",
            "CMAKE_BUILD_TYPE:STRING=Release\nCMAKE_TOOLCHAIN_FILE:FILEPATH=build/conan/conan_toolchain.cmake\n",
        );
        let cmake_args = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let recorded = cmake_args.clone();
        let spawn = move |cmd: &Command| -> Result<Box<dyn Spawned>, Error> {
            if cmd.program == "cmake" {
                *recorded.borrow_mut() = cmd.args.clone();
            }
            Ok(Box::new(FakeChild::done(if cmd.program == "cmake" {
                "-- Configuring done\n"
            } else {
                FMT_AND_ZLIB
            })))
        };
        let project = invocation(&host).project().expect("project");
        let task = host::begin_task("test", true);
        let outcome = apply_change(
            &project,
            &task,
            &spawn,
            Change::Install,
            "fmt",
            Some("12.2.0"),
        );
        assert!(outcome.ok, "{:?}", outcome.problem);
        assert!(
            cmake_args
                .borrow()
                .contains(&"-DCMAKE_BUILD_TYPE=Release".to_string()),
            "{:?}",
            cmake_args.borrow()
        );
        assert!(!cmake_args.borrow().contains(&"--fresh".to_string()));
    }

    #[test]
    fn a_change_uses_the_pinned_programs_through_start() {
        // The production spawn goes through host::start: the mock child prints
        // "started: <program>" and ends, which is all a smoke test of it needs.
        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "project.write",
            "process:conan",
            "process:cmake",
        ]);
        host.add_file("conanfile.txt", "[requires]\nzlib/1.3.1\n");
        host.add_file("CMakeLists.txt", "add_executable(app main.cpp)\n");
        let project = invocation(&host).project().expect("project");
        let task = host::begin_task("test", true);
        let outcome = apply_change(
            &project,
            &task,
            &host_spawn(&project),
            Change::Install,
            "fmt",
            Some("12.2.0"),
        );
        assert!(outcome.ok, "{:?}", outcome.problem);
        assert!(
            outcome.output.contains("$ conan install ."),
            "{:?}",
            outcome.output
        );
        assert!(
            outcome.output.contains("started: conan"),
            "{:?}",
            outcome.output
        );
        // Without Conan's words the recipe's own name is the find_package name.
        let lists = project.read_file("CMakeLists.txt").unwrap();
        assert!(lists.contains("find_package(fmt REQUIRED)"), "{lists}");
    }

    #[test]
    fn a_missing_process_grant_fails_the_change() {
        let denied = MockHost::install().with_capabilities(&["project.read", "project.write"]);
        denied.add_file("conanfile.txt", "[requires]\nzlib/1.3.1\n");
        let project = invocation(&denied).project().expect("project");
        let task = host::begin_task("test", true);
        let outcome = apply_change(
            &project,
            &task,
            &host_spawn(&project),
            Change::Install,
            "fmt",
            Some("12.2.0"),
        );
        assert!(!outcome.ok);
        assert!(outcome.problem.is_some());
    }

    #[test]
    fn a_conanfile_py_is_read_but_never_edited() {
        let host = MockHost::install().with_capabilities(&[
            "project.read",
            "project.write",
            "process:conan",
            "process:cmake",
        ]);
        host.add_file(
            "conanfile.py",
            "from conan import ConanFile\n\nclass App(ConanFile):\n    requires = \"fmt/12.0.0\"\n",
        );
        // With both files Conan uses the conanfile.py; so does the plugin.
        host.add_file("conanfile.txt", "[requires]\nzlib/1.3.1\n");
        host.add_file("CMakeLists.txt", "add_executable(app main.cpp)\n");
        let mut conan = Conan::default();
        conan
            .handle(
                VIEW,
                ViewEvent::Message(
                    json!({ "op": "install", "name": "zlib", "version": "1.3.1" }).to_string(),
                ),
                &invocation(&host),
            )
            .unwrap();
        let result = last(&host);
        assert_eq!(result["ok"], false);
        assert!(
            result["problem"].as_str().unwrap().contains("conanfile.py"),
            "{result}"
        );
        assert!(host.process_runs().is_empty());

        conan
            .handle(
                VIEW,
                ViewEvent::Message(json!({ "op": "search", "seq": 1, "query": "" }).to_string()),
                &invocation(&host),
            )
            .unwrap();
        let message = last(&host);
        assert_eq!(message["recipes"][0]["name"], "fmt");
        assert_eq!(message["recipes"][0]["installed"], "12.0.0");
    }

    #[test]
    fn the_view_starts_from_the_projects_recipe() {
        let host = MockHost::install().with_capabilities(&["project.read"]);
        host.add_file("conanfile.txt", "[requires]\nzlib/1.3.1\n");
        host.add_file("CMakeLists.txt", "add_executable(app main.cpp)\n");
        let mut conan = Conan::default();
        let View {
            body: Body::Html(page),
            ..
        } = conan.render(VIEW, None, &invocation(&host)).unwrap()
        else {
            panic!("an html body");
        };
        assert!(page.contains(r#""name":"project""#), "{page}");
        assert!(page.contains(r#""recipe":"conanfile.txt""#), "{page}");
        assert!(page.contains(r#""cmakeLists":true"#), "{page}");
        assert!(
            page.contains(r#""installed":[{"name":"zlib","version":"1.3.1"}]"#),
            "{page}"
        );
        assert!(!page.contains("__START__"));
    }
}
