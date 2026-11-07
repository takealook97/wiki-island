//! KorQuAD local-only evaluation. Never stores source text in report artifacts.
use anyhow::{Result, ensure};
use lindera::tokenizer::Tokenizer;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::time::Instant;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, TermQuery};
use tantivy::schema::{IndexRecordOption, STORED, Schema, TEXT};
use tantivy::{Term, doc};
use wi_bench::{Machine, Metric};
use wi_index::{Index, Query, literal_search};

#[derive(Deserialize)]
struct Dataset {
    data: Vec<Article>,
    version: String,
}
#[derive(Deserialize)]
struct Article {
    title: String,
    paragraphs: Vec<Paragraph>,
}
#[derive(Deserialize)]
struct Paragraph {
    context: String,
    qas: Vec<Question>,
}
#[derive(Deserialize)]
struct Question {
    question: String,
}
struct Case {
    text: String,
    gold: usize,
    short: bool,
}
#[derive(Default)]
struct Score {
    n: usize,
    recall: [usize; 3],
    reciprocal: f64,
    times: Vec<f64>,
}
impl Score {
    fn add(&mut self, ranked: &[usize], gold: usize, ms: f64) {
        self.n += 1;
        self.times.push(ms);
        if let Some(rank) = ranked.iter().position(|id| *id == gold) {
            for (i, k) in [1, 5, 10].iter().enumerate() {
                if rank < *k {
                    self.recall[i] += 1;
                }
            }
            if rank < 10 {
                self.reciprocal += 1.0 / (rank + 1) as f64;
            }
        }
    }
    fn value(&self) -> Result<serde_json::Value> {
        Ok(if self.n == 0 {
            json!({"queries":0,"recall_at_1":null,"recall_at_5":null,"recall_at_10":null,"mrr_at_10":null,"latency":null})
        } else {
            json!({"queries":self.n,"recall_at_1":self.recall[0] as f64/self.n as f64,"recall_at_5":self.recall[1] as f64/self.n as f64,"recall_at_10":self.recall[2] as f64/self.n as f64,"mrr_at_10":self.reciprocal/self.n as f64,"latency":Metric::samples(self.times.clone(),"ms",None)?})
        })
    }
}
fn korean_tokenizer() -> Result<Tokenizer> {
    // Do not inherit LINDERA_CONFIG_PATH or user token filters.
    Ok(Tokenizer::from_config(
        &json!({"segmenter":{"mode":"normal","dictionary":{"kind":"ko-dic"}},"character_filters":[],"token_filters":[]}),
    )?)
}

fn tokens(tokenizer: &Tokenizer, text: &str) -> Result<Vec<String>> {
    Ok(tokenizer
        .tokenize(text)?
        .into_iter()
        .map(|t| t.text.to_lowercase())
        .filter(|s| s.chars().any(char::is_alphanumeric))
        .collect())
}
fn hangul_length(text: &str) -> usize {
    text.chars()
        .filter(|c| ('\u{ac00}'..='\u{d7a3}').contains(c))
        .count()
}

pub fn run(dataset: &Path, output: &Path) -> Result<()> {
    let bytes = fs::read(dataset)?;
    ensure!(
        wi_history::hash_bytes(&bytes)
            == "sha256:25ffeb51e6c51ec02c071b60a10188e10005c144110f0d876b26079d80a35bdf",
        "dataset hash differs from pinned KorQuAD 1.0 dev download"
    );
    let parsed: Dataset = serde_json::from_slice(&bytes)?;
    ensure!(
        parsed.version == "KorQuAD_v1.0_dev",
        "unexpected corpus version"
    );
    let scratch = tempfile::tempdir()?;
    let root = scratch.path().join("passages");
    fs::create_dir(&root)?;
    let mut passages = Vec::<(String, String)>::new();
    let mut ids = HashMap::new();
    let mut cases = Vec::new();
    let mut probes = Vec::new();
    for article in parsed.data {
        for p in article.paragraphs {
            let id = match ids.get(&p.context) {
                Some(id) => *id,
                None => {
                    let id = passages.len();
                    ids.insert(p.context.clone(), id);
                    passages.push((article.title.clone(), p.context.clone()));
                    id
                }
            };
            for q in p.qas {
                let length = hangul_length(&q.question);
                cases.push(Case {
                    text: q.question.clone(),
                    gold: id,
                    short: (1..=2).contains(&length),
                });
                // Auxiliary probes, labelled separately: first 1–2 Hangul-only
                // syllable whitespace token in question also present in gold.
                if let Some(term) = q
                    .question
                    .split_whitespace()
                    .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()))
                    .find(|s| {
                        let n = s.chars().count();
                        (1..=2).contains(&n) && hangul_length(s) == n && p.context.contains(s)
                    })
                {
                    probes.push(Case {
                        text: term.into(),
                        gold: id,
                        short: true,
                    });
                }
            }
        }
    }
    for (id, (title, text)) in passages.iter().enumerate() {
        fs::write(
            root.join(format!("p{id:05}.md")),
            format!(
                "---\ntitle: {}\ntype: note\n---\n{text}\n",
                serde_json::to_string(title)?
            ),
        )?;
    }
    let mut fts = Index::open_at(&root, "korquad", scratch.path().join("fts.sqlite3"))?;
    let start = Instant::now();
    let sync = fts.rebuild()?;
    let fts_build_ms = start.elapsed().as_secs_f64() * 1000.0;
    ensure!(
        sync.skipped.is_empty(),
        "quality corpus has skipped passages"
    );
    let tokenizer = korean_tokenizer()?;
    let mut schema = Schema::builder();
    let text_field = schema.add_text_field("text", TEXT);
    let id_field = schema.add_u64_field("id", STORED);
    let schema = schema.build();
    let index_dir = scratch.path().join("tantivy");
    fs::create_dir(&index_dir)?;
    let index = tantivy::Index::create_in_dir(&index_dir, schema)?;
    let start = Instant::now();
    let mut writer = index.writer_with_num_threads(1, 50_000_000)?;
    for (id, (title, text)) in passages.iter().enumerate() {
        let terms = tokens(&tokenizer, &format!("{title}\n{text}"))?.join(" ");
        writer.add_document(doc!(text_field=>terms,id_field=>id as u64))?;
    }
    writer.commit()?;
    writer.wait_merging_threads()?;
    let tantivy_build_ms = start.elapsed().as_secs_f64() * 1000.0;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let mut scores: BTreeMap<String, Score> = BTreeMap::new();
    let mut query_errors = 0usize;
    for (set, queries) in [("questions", &cases), ("short_probes", &probes)] {
        for (number, case) in queries.iter().enumerate() {
            let bucket = if case.short {
                "1-2_syllables"
            } else {
                "longer"
            };
            let query = Query {
                limit: 10,
                ..Query::text(&case.text)
            };
            for engine in ["fts5_bigram", "tantivy_ko_or", "tantivy_ko_and", "literal"] {
                let start = Instant::now();
                let ranked = if engine == "fts5_bigram" || engine == "literal" {
                    let result = if engine == "literal" {
                        literal_search(&root, &query)?
                    } else {
                        fts.search(&query)?
                    };
                    if engine == "fts5_bigram" && result.fallback {
                        query_errors += 1;
                    }
                    result
                        .hits
                        .iter()
                        .filter_map(|h| {
                            h.rel_path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .and_then(|s| s.strip_prefix('p'))
                                .and_then(|s| s.parse::<usize>().ok())
                        })
                        .collect::<Vec<_>>()
                } else {
                    let mut terms = tokens(&tokenizer, &case.text)?;
                    terms.sort();
                    terms.dedup();
                    let occur = if engine == "tantivy_ko_and" {
                        Occur::Must
                    } else {
                        Occur::Should
                    };
                    let query = BooleanQuery::new(
                        terms
                            .into_iter()
                            .map(|s| {
                                (
                                    occur,
                                    Box::new(TermQuery::new(
                                        Term::from_field_text(text_field, &s),
                                        IndexRecordOption::WithFreqs,
                                    ))
                                        as Box<dyn tantivy::query::Query>,
                                )
                            })
                            .collect(),
                    );
                    let docs = searcher.search(&query, &TopDocs::with_limit(10))?;
                    let mut ranked = Vec::new();
                    for (_, address) in docs {
                        let doc: tantivy::TantivyDocument = searcher.doc(address)?;
                        let value = doc
                            .get_first(id_field)
                            .ok_or_else(|| anyhow::anyhow!("missing stored id"))?;
                        use tantivy::schema::Value;
                        ranked.push(
                            value
                                .as_u64()
                                .ok_or_else(|| anyhow::anyhow!("invalid stored id"))?
                                as usize,
                        );
                    }
                    ranked
                };
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                scores
                    .entry(format!("{set}/{engine}/all"))
                    .or_default()
                    .add(&ranked, case.gold, ms);
                scores
                    .entry(format!("{set}/{engine}/{bucket}"))
                    .or_default()
                    .add(&ranked, case.gold, ms);
            }
            if number % 250 == 0 {
                eprintln!("quality {set}: {number}/{}", queries.len());
            }
        }
    }
    for engine in ["fts5_bigram", "tantivy_ko_or", "tantivy_ko_and", "literal"] {
        scores
            .entry(format!("questions/{engine}/1-2_syllables"))
            .or_default();
    }
    let values = scores
        .iter()
        .map(|(k, v)| Ok((k.clone(), v.value()?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let size = |path: &Path| -> Result<u64> {
        let mut total = 0;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                total += entry.metadata()?.len();
            }
        }
        Ok(total)
    };
    let report = json!({"date":wi_bench::command_text("date", &["-u", "+%F"])?,"machine":Machine::detect()?,"dataset":{"version":parsed.version,"source":"https://korquad.github.io/dataset/KorQuAD_v1.0_dev.json","sha256":wi_history::hash_bytes(&bytes),"license":"CC BY-ND 2.0 KR; local use only; no corpus redistribution","passages":passages.len(),"questions":cases.len(),"short_probes":probes.len()},"engines":{"fts5":"wi-index default query builder + script bigram; raw question; all-terms matches first, then ranked bm25 OR over word terms and script n-grams for multi-term queries; limit 10","tantivy":"0.25.0 + lindera 0.43.3 ko-dic; surface tokens, lowercase, alphanumeric-containing tokens; no POS filter/stemming; pretokenized document title+body through default SimpleTokenizer; BM25 OR and AND controls; limit 10","literal":"wi-index literal_search, whole question substring; path order; no ranking"},"method":"Gold is the question's source passage, exact duplicate passages coalesced. All dev questions; no training/tuning. Short = total Hangul syllable count 1–2 in original question. Auxiliary probes are first 1–2-syllable Hangul whitespace term in question present in gold; not original labelled retrieval queries and other relevant passages are unlabelled. OS cache warm, sequential engines, single run; latency includes morphology and stored-id retrieval for Tantivy, filesystem parsing for literal. No claim of QA answer accuracy.","build_ms":{"fts5":fts_build_ms,"tantivy_including_morphology":tantivy_build_ms},"cache_bytes":{"fts_live":fs::metadata(scratch.path().join("fts.sqlite3"))?.len()+fs::metadata(scratch.path().join("fts.sqlite3-wal")).map(|m|m.len()).unwrap_or(0),"tantivy":size(&index_dir)?},"fts_fallback_queries":query_errors,"scores":values});
    fs::write(output, serde_json::to_string_pretty(&report)?)?;
    println!("quality results: {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_embedded_korean_tokenizer_is_usable() {
        let tokenizer = korean_tokenizer().unwrap();
        let terms = tokens(&tokenizer, "한국어 형태소 분석").unwrap();
        assert!(terms.len() >= 3);
        assert!(terms.iter().all(|s| !s.is_empty()));
    }
    #[test]
    fn ranks_use_one_gold_and_empty_buckets_stay_unknown() {
        let mut score = Score::default();
        score.add(&[9, 2, 4], 2, 1.0);
        score.add(&[3], 8, 2.0);
        let value = score.value().unwrap();
        assert_eq!(value["recall_at_1"], 0.0);
        assert_eq!(value["recall_at_5"], 0.5);
        assert_eq!(value["recall_at_10"], 0.5);
        assert_eq!(value["mrr_at_10"], 0.25);
        assert!(Score::default().value().unwrap()["mrr_at_10"].is_null());
        assert_eq!(hangul_length("환불 policy?"), 2);
    }
}
