use super::{open_ready_index, state_root, Coverage, SessionFields};
use serde::Serialize;
use std::collections::HashMap;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Value as TantivyValue};
use tantivy::{Index, TantivyDocument, Term};

const CURATED_LIMIT: usize = 3;

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub limit: usize,
    pub collapse: bool,
    pub final_only: bool,
    pub curated: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            collapse: true,
            final_only: false,
            curated: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResults {
    pub total: usize,
    pub hits: Vec<SearchHit>,
    pub curated_total: usize,
    pub curated_hits: Vec<CuratedHit>,
    pub built_at: Option<String>,
    pub coverage: Coverage,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateMember {
    pub document_id: String,
    pub session_id: String,
    pub turn_index: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub document_id: String,
    pub session_id: String,
    pub turn_index: usize,
    pub score: f32,
    pub snippet: String,
    pub harness: String,
    pub cwd: String,
    pub branch: String,
    pub model: String,
    pub timestamp: String,
    pub compacted: bool,
    pub partial: bool,
    pub duplicates: usize,
    pub duplicate_members: Vec<DuplicateMember>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CuratedHit {
    pub path: String,
    pub score: f32,
    pub snippet: String,
}

const SESSION_SEARCH_FIELDS: &[&str] = &[
    "document_id",
    "session_id",
    "harness",
    "cwd",
    "branch",
    "model",
    "timestamp",
    "turn_index",
    "compacted",
    "partial",
    "content_hash",
];
const CURATED_SEARCH_FIELDS: &[&str] = &["path"];

/// Preserve unknown `prefix:value` fragments as literal query text.
pub fn normalize_query(query: &str) -> String {
    normalize_query_for(query, SESSION_SEARCH_FIELDS)
}

fn normalize_query_for(query: &str, fields: &[&str]) -> String {
    query
        .split_whitespace()
        .map(|term| {
            let Some((prefix, _)) = term.split_once(':') else {
                return term.to_string();
            };
            if fields.contains(&prefix) {
                term.to_string()
            } else {
                format!("\"{}\"", term.replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Search the ready local indexes without invoking either writer.
pub fn search(query: &str, options: &SearchOptions) -> anyhow::Result<SearchResults> {
    let session_directory = state_root().join("sessions");
    let (index, metadata) = open_ready_index(&session_directory).map_err(|error| {
        anyhow::anyhow!("no readable session index; run `vaultr session index --update`: {error}")
    })?;
    let (total, hits) = search_session_index(&index, query, options)?;
    let (curated_total, curated_hits) = if options.curated {
        let (curated, _) = open_ready_index(&state_root().join("curated")).map_err(|error| {
            anyhow::anyhow!(
                "no readable curated index; run `vaultr session index --update`: {error}"
            )
        })?;
        search_curated_index(&curated, query)?
    } else {
        (0, Vec::new())
    };
    Ok(SearchResults {
        total,
        hits,
        curated_total,
        curated_hits,
        built_at: metadata.built_at,
        coverage: metadata.coverage,
    })
}

fn search_session_index(
    index: &Index,
    query_text: &str,
    options: &SearchOptions,
) -> anyhow::Result<(usize, Vec<SearchHit>)> {
    let schema = index.schema();
    let fields = SessionFields::load(&schema)?;
    let mut parser = QueryParser::for_index(index, vec![fields.body, fields.literal]);
    parser.set_conjunction_by_default();
    let parsed = parser.parse_query(&normalize_query(query_text))?;
    let query: Box<dyn Query> = if options.final_only {
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, parsed),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_bool(fields.compacted, false),
                    IndexRecordOption::Basic,
                )),
            ),
        ]))
    } else {
        parsed
    };
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let total = searcher.search(&*query, &tantivy::collector::Count)?;
    if total == 0 || options.limit == 0 {
        return Ok((total, Vec::new()));
    }
    let document_limit = if options.collapse {
        total
    } else {
        options.limit.min(total)
    };
    let top = searcher.search(
        &*query,
        &tantivy::collector::TopDocs::with_limit(document_limit),
    )?;
    let mut snippets = tantivy::SnippetGenerator::create(&searcher, &*query, fields.body)?;
    snippets.set_max_num_chars(480);
    let mut seen = HashMap::<String, usize>::new();
    let mut hits = Vec::<SearchHit>::new();
    for (score, address) in top {
        let document: TantivyDocument = searcher.doc(address)?;
        let content_hash = stored_string(&document, fields.content_hash);
        let member = DuplicateMember {
            document_id: stored_string(&document, fields.document_id),
            session_id: stored_string(&document, fields.session_id),
            turn_index: stored_u64(&document, fields.turn_index) as usize,
        };
        if options.collapse {
            if let Some(index) = seen.get(&content_hash).copied() {
                hits[index].duplicate_members.push(member);
                hits[index].duplicates = hits[index].duplicate_members.len();
                continue;
            }
            if hits.len() >= options.limit {
                continue;
            }
            seen.insert(content_hash, hits.len());
        }
        let snippet = snippets.snippet_from_doc(&document);
        let full_body = stored_string(&document, fields.body);
        hits.push(SearchHit {
            document_id: member.document_id.clone(),
            session_id: member.session_id.clone(),
            turn_index: member.turn_index,
            score,
            snippet: three_line_snippet(snippet.fragment(), &full_body),
            harness: stored_string(&document, fields.harness),
            cwd: stored_string(&document, fields.cwd),
            branch: stored_string(&document, fields.branch),
            model: stored_string(&document, fields.model),
            timestamp: stored_string(&document, fields.timestamp),
            compacted: stored_bool(&document, fields.compacted),
            partial: stored_bool(&document, fields.partial),
            duplicates: 1,
            duplicate_members: vec![member],
        });
        if !options.collapse && hits.len() >= options.limit {
            break;
        }
    }
    Ok((total, hits))
}

fn search_curated_index(
    index: &Index,
    query_text: &str,
) -> anyhow::Result<(usize, Vec<CuratedHit>)> {
    let schema = index.schema();
    let body = schema.get_field("body")?;
    let literal = schema.get_field("literal")?;
    let path = schema.get_field("path")?;
    let mut parser = QueryParser::for_index(index, vec![body, literal]);
    parser.set_conjunction_by_default();
    let query = parser.parse_query(&normalize_query_for(query_text, CURATED_SEARCH_FIELDS))?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let total = searcher.search(&*query, &tantivy::collector::Count)?;
    if total == 0 {
        return Ok((0, Vec::new()));
    }
    let top = searcher.search(
        &*query,
        &tantivy::collector::TopDocs::with_limit(CURATED_LIMIT.min(total)),
    )?;
    let mut snippets = tantivy::SnippetGenerator::create(&searcher, &*query, body)?;
    snippets.set_max_num_chars(480);
    let hits = top
        .into_iter()
        .map(|(score, address)| -> anyhow::Result<CuratedHit> {
            let document: TantivyDocument = searcher.doc(address)?;
            let snippet = snippets.snippet_from_doc(&document);
            let full_body = stored_string(&document, body);
            Ok(CuratedHit {
                path: stored_string(&document, path),
                score,
                snippet: three_line_snippet(snippet.fragment(), &full_body),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((total, hits))
}

fn stored_string(document: &TantivyDocument, field: Field) -> String {
    document
        .get_first(field)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn stored_u64(document: &TantivyDocument, field: Field) -> u64 {
    document
        .get_first(field)
        .and_then(|value| value.as_u64())
        .unwrap_or_default()
}

fn stored_bool(document: &TantivyDocument, field: Field) -> bool {
    document
        .get_first(field)
        .and_then(|value| value.as_bool())
        .unwrap_or_default()
}

fn three_line_snippet(fragment: &str, full_body: &str) -> String {
    let snippet = fragment
        .lines()
        .take(3)
        .map(|line| truncate_chars(line, 240))
        .collect::<Vec<_>>()
        .join("\n");
    if snippet != full_body {
        return snippet;
    }
    let visible = snippet.chars().count().saturating_sub(1);
    let mut shortened: String = snippet.chars().take(visible).collect();
    shortened.push('…');
    shortened
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut output: String = text.chars().take(limit.saturating_sub(1)).collect();
    output.push('…');
    output
}
