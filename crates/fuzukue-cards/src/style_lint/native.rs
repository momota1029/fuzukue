use super::*;

const REGISTER: &str = "style/no-mix-dearu-desumasu";
const PERIOD: &str = "style/ja-no-mixed-period";
const KANJI: &str = "style/max-kanji-continuous-len";
const JOSHI: &str = "style/no-doubled-joshi";
const DOUBLE_NEGATIVE: &str = "style/no-double-negative-ja";
const WEAK: &str = "style/ja-no-weak-phrase";
const REDUNDANT: &str = "style/ja-no-redundant-expression";
const ASCII_COMMAS: &str = "style/max-comma";
const EXCLAMATION: &str = "style/no-exclamation-question-mark";
const HANKAKU: &str = "style/no-hankaku-kana";

pub(super) fn alias(code: &str) -> Option<&str> {
    match code {
        AI => Some("no-ai-hype-expressions"),
        LONG => Some("sentence-length"),
        COMMA => Some("max-ten"),
        REGISTER | PERIOD | KANJI | JOSHI | DOUBLE_NEGATIVE | WEAK | REDUNDANT | ASCII_COMMAS
        | EXCLAMATION | HANKAKU => code.strip_prefix("style/"),
        _ => None,
    }
}

static KANJI_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{Han}々〆ヶ]+").unwrap());
// Only count particles immediately following a noun-like run. Kana-only words
// require morphological analysis to distinguish particles from word endings.
static PARTICLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{Han}\p{Katakana}A-Za-z0-9々]+([はがをにでのへも])").unwrap());
static POLITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:です|でした|でしょう|でございます|ません|[きぎしちにびみりえけせてねべめれい]ま(?:す|した|しょう))$").unwrap()
});
static PLAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:である|であった|であろう|なのだ|のだ|する|した|しない|される|された|できる|となる|がある|がない|ている|ていた|^.+[\p{Han}\p{Katakana}]だ)$").unwrap()
});
static DOUBLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:なく[はも]|ないで[はも]|ない(?:こと|事)[はも]|ない(?:もの|わけ)で[はも]|ない(?:と|なければ)(?:いけ|なら))(?:ない|なかった|ありません)").unwrap()
});
static WEAK_PHRASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"かもしれ(?:ない|ません)|(?:と|ように)思いま(?:す|した)|(?:と|ように)思う|(?:だ|である)ろうと思").unwrap()
});
static REDUNDANCY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"すること[がはも](?:でき(?:る|ます|た)|(?:不)?可能)|であると言え(?:る|ます)|であると考えてい(?:る|ます)|(?:検討|確認|調査|計算)を(?:行[ういっ]|実行)").unwrap()
});
static HANKAKU_KANA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\u{FF66}-\u{FF9F}]+").unwrap());
static EMPHATIC_PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[!！?？]+").unwrap());

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Register {
    Auto,
    Plain,
    Polite,
}

#[derive(Default)]
pub(super) struct RegisterUse {
    plain: usize,
    polite: usize,
    first: Option<Register>,
}

impl RegisterUse {
    pub(super) fn observe(&mut self, sentence: &str) {
        if let Some((register, _)) = register(sentence) {
            self.first.get_or_insert(register);
            match register {
                Register::Plain => self.plain += 1,
                Register::Polite => self.polite += 1,
                Register::Auto => unreachable!("auto is a preference"),
            }
        }
    }

    pub(super) fn preferred(&self, preference: Register) -> Option<Register> {
        match preference {
            Register::Auto => match self.plain.cmp(&self.polite) {
                std::cmp::Ordering::Greater => Some(Register::Plain),
                std::cmp::Ordering::Less => Some(Register::Polite),
                std::cmp::Ordering::Equal => self.first,
            },
            explicit => Some(explicit),
        }
    }
}

fn register(sentence: &str) -> Option<(Register, &str)> {
    if sentence.ends_with(['」', '』', '”', '"']) {
        return None;
    }
    let stem = sentence.trim_end_matches(['。', '．', '.', '！', '？', '!', '?']);
    POLITE
        .find(stem)
        .map(|m| (Register::Polite, m.as_str()))
        .or_else(|| PLAIN.find(stem).map(|m| (Register::Plain, m.as_str())))
}

pub(super) fn inspect_sentence<'a>(
    sentence: &'a str,
    config: &StyleConfig,
    preferred: Option<Register>,
    enabled: &impl Fn(&str) -> bool,
    warn: &mut impl FnMut(&'static str, String, &'a str),
) {
    if enabled(REGISTER) {
        if let Some((register, ending)) = register(sentence) {
            if preferred.is_some_and(|p| p != register) {
                warn(REGISTER, quoted(sentence), ending);
            }
        }
    }
    if enabled(PERIOD) && JAPANESE.is_match(sentence) {
        let text = sentence.trim_end_matches(['」', '』', '）', ')', '”', '’', '"']);
        let period = config
            .period_mark
            .as_deref()
            .unwrap_or(&DEFAULTS.period_mark);
        if let Some((i, mark)) = text.char_indices().next_back() {
            if matches!(mark, '。' | '．' | '.') && &text[i..] != period {
                warn(PERIOD, quoted(sentence), &text[i..]);
            }
        }
    }
    if enabled(KANJI) {
        let limit = config.max_kanji_chars.unwrap_or(DEFAULTS.max_kanji_chars);
        for found in KANJI_RUN.find_iter(sentence) {
            let run = found.as_str();
            if run.chars().count() > limit {
                warn(KANJI, quoted(run), run);
            }
        }
    }
    if enabled(JOSHI) {
        let mut previous = None;
        for capture in PARTICLE.captures_iter(sentence) {
            let found = capture.get(1).unwrap();
            let particle = found.as_str();
            let rest = &sentence[found.end()..];
            if particle == "で"
                && ["は", "も", "ある", "あっ", "あろ", "あり", "なく", "ない"]
                    .iter()
                    .any(|suffix| rest.starts_with(suffix))
            {
                previous = None;
                continue;
            }
            if previous == Some(particle) {
                warn(JOSHI, quoted(sentence), particle);
            }
            if let Some(duplicate) = rest.get(..particle.len()).filter(|next| *next == particle) {
                warn(JOSHI, quoted(sentence), duplicate);
            }
            previous = Some(particle);
        }
    }
    if enabled(ASCII_COMMAS) {
        let limit = config.max_ascii_commas.unwrap_or(DEFAULTS.max_ascii_commas);
        if let Some((start, comma)) = sentence.match_indices(',').nth(limit) {
            warn(
                ASCII_COMMAS,
                quoted(sentence),
                &sentence[start..start + comma.len()],
            );
        }
    }
    for (code, pattern) in [
        (DOUBLE_NEGATIVE, &*DOUBLE),
        (WEAK, &*WEAK_PHRASE),
        (REDUNDANT, &*REDUNDANCY),
        (EXCLAMATION, &*EMPHATIC_PUNCTUATION),
        (HANKAKU, &*HANKAKU_KANA),
    ] {
        if enabled(code) {
            for found in pattern.find_iter(sentence) {
                warn(code, quoted(found.as_str()), found.as_str());
            }
        }
    }
}
