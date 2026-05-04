// text-analyzer/src/lib.rs
wit_bindgen::generate!({
    world: "analyzer",
    path: "../wit",
});

use exports::example::text::analyze::{
    Guest, Token, Analysis, AnalyzeError,
};

struct Analyzer;

const POSITIVE: &[&str] = &["good", "great", "excellent", "love", "happy", "wonderful"];
const NEGATIVE: &[&str] = &["bad", "terrible", "awful", "hate", "sad", "horrible"];
const STOPWORDS: &[&str] = &["the", "a", "an", "is", "are", "was", "were", "and", "or"];

impl Guest for Analyzer {
    fn analyze_text(input: String, max_keywords: u32) -> Result<Analysis, AnalyzeError> {
        if input.is_empty() {
            return Err(AnalyzeError::EmptyInput);
        }
        if input.len() > 1_000_000 {
            return Err(AnalyzeError::TooLarge(input.len() as u32));
        }

        // Tokenize: find word boundaries, record byte offsets
        let mut tokens = Vec::new();
        let mut start: Option<u32> = None;
        for (i, c) in input.char_indices() {
            if c.is_alphanumeric() {
                if start.is_none() {
                    start = Some(i as u32);
                }
            } else if let Some(s) = start.take() {
                tokens.push(Token {
                    text: input[s as usize..i].to_lowercase(),
                    start: s,
                    end: i as u32,
                });
            }
        }
        if let Some(s) = start {
            tokens.push(Token {
                text: input[s as usize..].to_lowercase(),
                start: s,
                end: input.len() as u32,
            });
        }

        let word_count = tokens.len() as u32;

        // Naive sentiment: count positive vs negative words
        let mut score: i32 = 0;
        for t in &tokens {
            if POSITIVE.contains(&t.text.as_str()) { score += 1; }
            if NEGATIVE.contains(&t.text.as_str()) { score -= 1; }
        }
        let sentiment = if word_count == 0 {
            0.0
        } else {
            (score as f32) / (word_count as f32).sqrt()
        }.clamp(-1.0, 1.0);

        // Keywords: most-frequent non-stopwords
        let mut counts: std::collections::HashMap<String, u32> = Default::default();
        for t in &tokens {
            if !STOPWORDS.contains(&t.text.as_str()) && t.text.len() > 2 {
                *counts.entry(t.text.clone()).or_insert(0) += 1;
            }
        }
        let mut sorted: Vec<_> = counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let keywords = sorted.into_iter()
            .take(max_keywords as usize)
            .map(|(w, _)| w)
            .collect();

        Ok(Analysis { tokens, word_count, sentiment, keywords })
    }
}

export!(Analyzer);