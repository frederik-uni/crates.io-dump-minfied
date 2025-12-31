use chrono::{DateTime, Utc};
use db_dump::categories::CategoryId;
use db_dump::crates::{CrateId, Row};
use db_dump::keywords::KeywordId;
use db_dump::versions::VersionId;
use reqwest::Client;
use std::cmp::Reverse;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap as Map, BTreeSet as Set};
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::process::exit;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("Usage: dump \"date here\"");
        exit(1);
    }
    let update = download_if_updated(args.get(1).unwrap()).await;
    let data = process().unwrap();
    let mut file = File::create("dump").unwrap();
    for crat in data {
        let by = crat.to_vec();
        file.write_all(&(by.len() as u32).to_le_bytes()).unwrap();
        file.write_all(&by).unwrap();
    }
    File::create("last_updated")
        .unwrap()
        .write_all(update.as_bytes())
        .unwrap();
}

async fn download_if_updated(last: &str) -> String {
    let url = "https://static.crates.io/db-dump.tar.gz";
    let client = Client::new();

    let local_last_modified: Option<DateTime<Utc>> = DateTime::parse_from_rfc2822(last.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc));

    let resp = client.head(url).send().await.unwrap();
    let remote_last_modified = resp
        .headers()
        .get("Last-Modified")
        .and_then(|hdr| hdr.to_str().ok())
        .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    if let Some(remote_date) = remote_last_modified {
        let should_download = match local_last_modified {
            Some(local_date) => remote_date > local_date,
            None => true,
        };

        if should_download {
            println!("New version available. Downloading...");

            let file_resp = client.get(url).send().await.unwrap();
            let bytes = file_resp.bytes().await.unwrap();
            fs::write("db-dump.tar.gz", &bytes).unwrap();

            println!("Download complete and last_updated file updated.");
            return remote_date.to_rfc2822().to_string();
        } else {
            println!("No update needed. Already up to date.");

            exit(20)
        }
    } else {
        println!("No Last-Modified header found. Skipping download.");
        exit(21)
    }
}

fn process() -> db_dump::Result<Vec<Crate>> {
    // Map of crate id to the most recently published version of that crate.
    let mut most_recent = Map::new();

    let mut crates = Set::new();
    let mut dependencies = Vec::new();
    let mut crate_keywords: Map<CrateId, Vec<KeywordId>> = Map::new();
    let mut crate_categories: Map<CrateId, Vec<CategoryId>> = Map::new();
    let mut all_keywords: Map<KeywordId, String> = Map::new();
    let mut all_categories: Map<CategoryId, String> = Map::new();
    let mut version_count = Map::<CrateId, u32>::new();
    let mut libs = Set::<CrateId>::new();
    let mut stable_versions = Map::<CrateId, semver::Version>::new();
    let mut versions = Map::<CrateId, semver::Version>::new();
    db_dump::Loader::new()
        .crates(|row| {
            crates.insert(row);
        })
        .dependencies(|row| dependencies.push(row))
        .versions(|row| {
            let v = &row.num;
            match v.pre.is_empty() {
                true => {
                    stable_versions
                        .entry(row.crate_id)
                        .and_modify(|old_version| {
                            if *old_version < *v {
                                *old_version = v.clone();
                            }
                        })
                        .or_insert(v.clone());
                }
                false => {
                    versions
                        .entry(row.crate_id)
                        .and_modify(|old_version| {
                            if *old_version < *v {
                                *old_version = v.clone();
                            }
                        })
                        .or_insert(v.clone());
                }
            };
            if row.has_lib {
                libs.insert(row.crate_id);
            }
            match most_recent.entry(row.crate_id) {
                Entry::Vacant(entry) => {
                    entry.insert(row);
                }
                Entry::Occupied(mut entry) => {
                    if row.created_at > entry.get().created_at {
                        entry.insert(row);
                    }
                }
            }
        })
        .default_versions(|row| {
            version_count.insert(row.crate_id, row.num_versions.unwrap_or_default());
        })
        .crates_keywords(|row| {
            crate_keywords
                .entry(row.crate_id)
                .or_default()
                .push(row.keyword_id);
        })
        .crates_categories(|row| {
            crate_categories
                .entry(row.crate_id)
                .or_default()
                .push(row.category_id);
        })
        .keywords(|row| {
            all_keywords.insert(row.id, row.keyword.clone());
        })
        .categories(|row| {
            all_categories.insert(row.id, row.category.clone());
        })
        .load("./db-dump.tar.gz")?;
    let crates = crates
        .into_iter()
        .filter(|c| libs.contains(&c.id))
        .collect::<Set<Row>>();

    // Set of version ids which are the most recently published of their crate.
    let most_recent = Set::from_iter(most_recent.values().map(|version| version.id));

    // Set of (version id, dependency crate id) pairs to avoid double-counting
    // cases where a crate has both a normal dependency and dev-dependency or
    // build-dependency on the same dependency crate.
    let mut unique_dependency_edges = Set::<(VersionId, CrateId)>::new();

    // Map of crate id to how many other crates' most recent version depends on that crate.
    let mut count = Map::<CrateId, usize>::new();
    for dep in dependencies {
        if most_recent.contains(&dep.version_id)
            && unique_dependency_edges.insert((dep.version_id, dep.crate_id))
        {
            *count.entry(dep.crate_id).or_default() += 1;
        }
    }

    for crate_id in &crates {
        count.entry(crate_id.id).or_insert(0);
    }

    // Optional: Sort all crates by count descending
    let mut all_crates: Vec<_> = count.into_iter().collect();
    all_crates.sort_unstable_by_key(|&(_, count)| Reverse(count));
    let mut keywords = File::create("keywords").unwrap();
    keywords
        .write_all(
            all_keywords
                .into_iter()
                .map(|v| {
                    let mut bytes: Vec<u8> = vec![];
                    bytes.extend(&v.0.0.to_le_bytes());
                    bytes.extend(&(v.1.len() as u32).to_le_bytes());
                    bytes.extend(v.1.as_bytes());
                    bytes
                })
                .flatten()
                .collect::<Vec<u8>>()
                .as_slice(),
        )
        .unwrap();
    let mut categories = File::create("categories").unwrap();
    categories
        .write_all(
            all_categories
                .into_iter()
                .map(|v| {
                    let mut bytes: Vec<u8> = vec![];
                    bytes.extend(&v.0.0.to_le_bytes());
                    bytes.extend(&(v.1.len() as u32).to_le_bytes());
                    bytes.extend(v.1.as_bytes());
                    bytes
                })
                .flatten()
                .collect::<Vec<u8>>()
                .as_slice(),
        )
        .unwrap();
    let mut out = vec![];
    for (id, count) in all_crates {
        let crat = &crates.get(&id);
        if let Some(crat) = crat {
            out.push(Crate {
                order: count as u32,
                name: crat.name.clone(),
                repository: crat.repository.clone(),
                homepage: crat.homepage.clone(),
                documentation: crat.documentation.clone(),
                description: crat.description.clone(),
                latest_stable_version: stable_versions.get(&crat.id).map(|v| v.to_string()),
                latest_version: versions.get(&crat.id).map(|v| v.to_string()),
                categories: crate_categories
                    .get(&crat.id)
                    .map(|v| v.into_iter().map(|v| v.0).collect())
                    .unwrap_or_default(),
                keywords: crate_keywords
                    .get(&crat.id)
                    .map(|v| v.into_iter().map(|v| v.0).collect())
                    .unwrap_or_default(),
                num_versions: version_count.get(&crat.id).map(|v| *v).unwrap_or_default(),
            });
        }
    }
    Ok(out)
}

//keyword file, categories file
#[derive(Debug)]
pub struct Crate {
    name: String,
    repository: Option<String>,
    homepage: Option<String>,
    documentation: Option<String>,
    description: String,
    latest_stable_version: Option<String>,
    latest_version: Option<String>,
    categories: Vec<u32>,
    keywords: Vec<u32>,
    num_versions: u32,
    order: u32,
}

macro_rules! read_u32 {
    ($data:expr, $cursor:expr) => {{
        let value = u32::from_le_bytes($data[$cursor..$cursor + 4].try_into().unwrap());
        $cursor += 4;
        value
    }};
}

macro_rules! read_string {
    ($data:expr, $cursor:expr) => {{
        let len = read_u32!($data, $cursor);
        let s = String::from_utf8($data[$cursor..$cursor + len as usize].to_vec()).unwrap();
        $cursor += len as usize;
        s
    }};
}

impl Crate {
    pub fn to_vec(self) -> Vec<u8> {
        let mut byte_array = Vec::new();

        byte_array.extend(&self.order.to_le_bytes());
        byte_array.extend(&self.num_versions.to_le_bytes());
        byte_array.extend(&(self.keywords.len() as u32).to_le_bytes());
        for keyword in &self.keywords {
            byte_array.extend(&keyword.to_le_bytes());
        }
        byte_array.extend(&(self.categories.len() as u32).to_le_bytes());
        for keyword in &self.categories {
            byte_array.extend(&keyword.to_le_bytes());
        }
        let mut add_str = |s: &str| {
            byte_array.extend(&(s.len() as u32).to_le_bytes());
            byte_array.extend(s.as_bytes());
        };
        add_str(&self.name);
        add_str(&self.description);
        add_str(&self.repository.unwrap_or_default());
        add_str(&self.homepage.unwrap_or_default());
        add_str(&self.documentation.unwrap_or_default());
        add_str(&self.latest_stable_version.unwrap_or_default());
        add_str(&self.latest_version.unwrap_or_default());
        byte_array
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        let mut cursor = 0;

        let order = read_u32!(data, cursor);
        let num_versions = read_u32!(data, cursor);

        let keywords_len = read_u32!(data, cursor) as usize;
        let mut keywords = Vec::with_capacity(keywords_len);
        for _ in 0..keywords_len {
            keywords.push(read_u32!(data, cursor));
        }

        let categories_len = read_u32!(data, cursor) as usize;
        let mut categories = Vec::with_capacity(categories_len);
        for _ in 0..categories_len {
            categories.push(read_u32!(data, cursor));
        }

        let name = read_string!(data, cursor);
        let description = read_string!(data, cursor);
        let repository = if !data[cursor..].is_empty() {
            let str = read_string!(data, cursor);
            match str.len() == 0 {
                true => None,
                false => Some(str),
            }
        } else {
            unreachable!()
        };
        let homepage = if !data[cursor..].is_empty() {
            let str = read_string!(data, cursor);
            match str.len() == 0 {
                true => None,
                false => Some(str),
            }
        } else {
            unreachable!()
        };
        let documentation = if !data[cursor..].is_empty() {
            let str = read_string!(data, cursor);
            match str.len() == 0 {
                true => None,
                false => Some(str),
            }
        } else {
            unreachable!();
        };
        let latest_stable_version = if !data[cursor..].is_empty() {
            let str = read_string!(data, cursor);
            match str.len() == 0 {
                true => None,
                false => Some(str),
            }
        } else {
            unreachable!()
        };
        let latest_version = if !data[cursor..].is_empty() {
            #[allow(unused_assignments)]
            let str = read_string!(data, cursor);
            match str.len() == 0 {
                true => None,
                false => Some(str),
            }
        } else {
            unreachable!()
        };

        Self {
            order,
            num_versions,
            keywords,
            categories,
            name,
            description,
            repository,
            homepage,
            documentation,
            latest_stable_version,
            latest_version,
        }
    }
}
