//! Consume the retired Super+Alt+C chord without running any control action.
//! Legacy Alt+C remains copy mode; unknown CSI-u sequences keep their old path.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decoded {
    Pending,
    Ignore,
    Other,
}

pub(super) fn decode(bytes: &[u8]) -> Decoded {
    const PREFIX: &[u8] = b"\x1b[";
    if PREFIX.starts_with(bytes) {
        return Decoded::Pending;
    }
    let Some(body) = bytes.strip_prefix(PREFIX) else {
        return Decoded::Other;
    };
    if body.len() > 64 {
        return Decoded::Other;
    }
    let complete = body.last() == Some(&b'u');
    let digits = if complete {
        &body[..body.len() - 1]
    } else {
        body
    };
    if !digits
        .iter()
        .all(|byte| byte.is_ascii_digit() || matches!(*byte, b';' | b':'))
    {
        return Decoded::Other;
    }
    let key_end = digits
        .iter()
        .position(|byte| matches!(*byte, b';' | b':'))
        .unwrap_or(digits.len());
    let key = &digits[..key_end];
    if !complete
        && key_end == digits.len()
        && [b"99".as_slice(), b"67".as_slice()]
            .iter()
            .any(|code| code.starts_with(key))
    {
        return Decoded::Pending;
    }
    if key != b"99" && key != b"67" {
        return Decoded::Other;
    }
    if !complete {
        return Decoded::Pending;
    }
    let Ok(text) = std::str::from_utf8(digits) else {
        return Decoded::Other;
    };
    let mut fields = text.split(';');
    let Some(key) = fields.next() else {
        return Decoded::Other;
    };
    if key.split(':').count() > 3 {
        return Decoded::Other;
    }
    let Some(modifiers) = fields.next() else {
        return Decoded::Other;
    };
    let mut modifiers = modifiers.split(':');
    let Some(bits) = modifiers
        .next()
        .and_then(|field| field.parse::<u16>().ok())
        .and_then(|value| value.checked_sub(1))
    else {
        return Decoded::Other;
    };
    // Kitty modifier bits: Alt=2, Super=8, CapsLock=64, NumLock=128.
    // Never swallow Shift/Control/Hyper/Meta or plain Alt+C as the old chord.
    if bits & !(64 | 128) != (2 | 8) {
        return Decoded::Other;
    }
    let event = match modifiers.next() {
        None => 1,
        Some(value) => match value.parse::<u8>() {
            Ok(value) => value,
            Err(_) => return Decoded::Other,
        },
    };
    if modifiers.next().is_some() || fields.count() > 1 {
        return Decoded::Other;
    }
    match event {
        1..=3 => Decoded::Ignore,
        _ => Decoded::Other,
    }
}
