//! Plain, editable quotations shared by native conversation clients.
const EXCERPT_BYTES: usize = 2048;
pub fn valid_message_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}
/// Preserve the current draft and send quotations through the ordinary text path.
pub fn quote_reply(id: &str, text: &str, draft: &str, maximum: usize) -> Result<String, String> {
    if !valid_message_id(id) {
        return Err("This message has no stable original ID to quote.".into());
    }
    let maximum = maximum.min(crate::wire::MAX_USER_MESSAGE_BYTES);
    let suffix = "\n> …（引用已截断）";
    let mut quote = format!("> 回复消息 {id}\n> ");
    let reserve = draft.len().saturating_add(2).saturating_add(suffix.len());
    if quote.len().saturating_add(reserve) > maximum {
        return Err("The draft has insufficient space for this quote.".into());
    }
    let mut used = 0;
    let mut truncated = false;
    for ch in text
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
    {
        let addition = if ch == '\n' { 3 } else { ch.len_utf8() };
        if used + ch.len_utf8() > EXCERPT_BYTES || quote.len() + addition + reserve > maximum {
            truncated = true;
            break;
        }
        if ch == '\n' {
            quote.push_str("\n> ");
        } else {
            quote.push(ch);
        }
        used += ch.len_utf8();
    }
    if truncated {
        quote.push_str(suffix);
    }
    quote.push_str("\n\n");
    quote.push_str(draft);
    Ok(quote)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quotes_preserve_ids_unicode_drafts_and_whole_message_budget() {
        let draft = "已有回答 🦀";
        let value = quote_reply("message_original", &"中文🦀\n".repeat(900), draft, 4096).unwrap();
        assert!(value.starts_with("> 回复消息 message_original\n> 中文🦀"));
        assert!(value.contains("引用已截断"));
        assert!(value.ends_with(draft));
        assert!(value.len() <= 4096);
        assert!(
            value
                .split("\n\n")
                .next()
                .unwrap()
                .lines()
                .all(|line| line.starts_with('>'))
        );
        assert!(quote_reply("message_original", "quoted", &"x".repeat(4096), 4096).is_err());
        assert!(quote_reply("bad\nid", "quoted", "", 4096).is_err());
    }
}
