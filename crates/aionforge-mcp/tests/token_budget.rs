//! Token-budget regression tests for advertised MCP metadata.

const MCP_LIB_RS: &str = include_str!("../src/lib.rs");
const MAX_TOOL_DESCRIPTION_CHARS: usize = 120;
const MAX_TOOL_DESCRIPTION_WORDS: usize = 16;
const MAX_TOTAL_DESCRIPTION_CHARS: usize = 970;
const MAX_TOTAL_DESCRIPTION_WORDS: usize = 132;

#[test]
fn mcp_tool_descriptions_stay_compact() {
    let descriptions = tool_descriptions(MCP_LIB_RS);
    assert_eq!(
        descriptions.len(),
        11,
        "parsed unexpected tool descriptions: {descriptions:?}"
    );

    let total_chars: usize = descriptions.iter().map(String::len).sum();
    let total_words: usize = descriptions
        .iter()
        .map(|description| description.split_whitespace().count())
        .sum();

    assert!(
        total_chars <= MAX_TOTAL_DESCRIPTION_CHARS,
        "tool descriptions use {total_chars} chars; cap is {MAX_TOTAL_DESCRIPTION_CHARS}"
    );
    assert!(
        total_words <= MAX_TOTAL_DESCRIPTION_WORDS,
        "tool descriptions use {total_words} words; cap is {MAX_TOTAL_DESCRIPTION_WORDS}"
    );

    for description in descriptions {
        assert!(
            description.len() <= MAX_TOOL_DESCRIPTION_CHARS,
            "tool description too long ({} chars): {description}",
            description.len()
        );
        let words = description.split_whitespace().count();
        assert!(
            words <= MAX_TOOL_DESCRIPTION_WORDS,
            "tool description too wordy ({words} words): {description}"
        );
    }
}

fn tool_descriptions(src: &str) -> Vec<String> {
    let mut descriptions = Vec::new();
    let mut rest = src;
    while let Some(tool_start) = rest.find("#[tool(") {
        rest = &rest[tool_start + "#[tool(".len()..];
        let Some(tool_end) = rest.find(")]") else {
            break;
        };
        let tool_attr = &rest[..tool_end];
        if let Some(description) = description_literal(tool_attr) {
            descriptions.push(description);
        }
        rest = &rest[tool_end + ")]".len()..];
    }
    descriptions
}

fn description_literal(attr: &str) -> Option<String> {
    let start = attr.find("description")?;
    let after_description = &attr[start + "description".len()..];
    let eq = after_description.find('=')?;
    let after_eq = after_description[eq + 1..].trim_start();
    let after_quote = after_eq.strip_prefix('"')?;
    let end = after_quote.find('"')?;
    Some(after_quote[..end].to_string())
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn parser_handles_multiline_tool_attributes() {
        let src = r#"
            #[tool(
                description = "Short read tool."
            )]
            async fn read_it(&self) {}

            #[tool(description = "Another concise tool.")]
            async fn write_it(&self) {}
        "#;
        assert_eq!(
            tool_descriptions(src),
            vec!["Short read tool.", "Another concise tool."]
        );
    }
}
