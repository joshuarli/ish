use epsh::ast::WordPart;

pub(crate) fn static_word(parts: &[WordPart]) -> Option<String> {
    fn all_static(parts: &[WordPart]) -> bool {
        parts.iter().all(|part| match part {
            WordPart::Literal(_) | WordPart::SingleQuoted(_) | WordPart::Tilde(_) => true,
            WordPart::DoubleQuoted(inner) => all_static(inner),
            _ => false,
        })
    }

    all_static(parts).then(|| epsh::lexer::parts_to_text(parts))
}
