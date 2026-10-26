//! Classify an upstream video submission failure into a stable kind plus one
//! actionable sentence.
//!
//! Upstream rejects a submission for reasons the caller can fix: a reference
//! image that trips the real-person check, an asset it cannot download, an
//! unsupported duration. The vendor text still ships unchanged with the result;
//! this only adds a label so a caller can show one clear line instead of an
//! opaque wall of provider prose.

pub(super) fn classify(error: &str) -> Option<(&'static str, &'static str)> {
    if error.contains("may contain real person") {
        return Some((
            "reference_contains_person",
            "参考图被上游判定含真人：请把人物图放到 1 号位，或换用不含真人的图片",
        ));
    }
    let lower = error.to_ascii_lowercase();
    if lower.contains("download failed") || lower.contains("resource download") {
        return Some((
            "reference_unreachable",
            "参考素材无法被上游下载：请确认链接可从公网访问，或改用本地上传",
        ));
    }
    if error.contains("INVALID_DURATION") || lower.contains("duration exceeds") {
        return Some((
            "duration_unsupported",
            "时长参数不被支持：请缩短时长或改用允许的档位",
        ));
    }
    if lower.contains("content policy") || lower.contains("sensitive") {
        return Some((
            "content_policy",
            "内容被上游安全策略拦截：请调整描述或素材",
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn classifies_actionable_failures() {
        let person = "model call failed: LLMRawChatV2 error: code=3003 message=[callArkVideoGeneration] failed: Error code: 400 - {\"message\":\"The request failed because the input image 'content[1]' may contain real person\"}";
        assert_eq!(classify(person).map(|c| c.0), Some("reference_contains_person"));
        assert_eq!(classify("resource download failed").map(|c| c.0), Some("reference_unreachable"));
        assert_eq!(classify("Reference duration exceeds maximum").map(|c| c.0), Some("duration_unsupported"));
        assert_eq!(classify("something unexpected").map(|c| c.0), None);
    }
}
