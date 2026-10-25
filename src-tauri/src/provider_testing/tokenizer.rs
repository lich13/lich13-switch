//! Port of the pinned LM Detector tokenizer posterior and serial probe selection.
//! See resources/lm-detector/LICENSE.txt and vendor/lmfpd/UPSTREAM.
use super::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    sync::OnceLock,
    time::Duration,
};

// The fixed bank uses four terminal negative lookaheads. Compile their suffix
// checks separately, preserving JavaScript's match-end semantics without a new regex engine.
struct Pattern {
    base: Regex,
    excluded: Option<Regex>,
}
impl Pattern {
    fn new(pattern: &str) -> Result<Self> {
        let compile = |s: &str| {
            RegexBuilder::new(s)
                .case_insensitive(true)
                .build()
                .map_err(|_| failure("分词器型号规则不兼容"))
        };
        if let Some((base, suffix)) = pattern.rsplit_once("(?!") {
            let suffix = suffix
                .strip_suffix(')')
                .ok_or_else(|| failure("分词器型号规则不兼容"))?;
            Ok(Self {
                base: compile(base)?,
                excluded: Some(compile(&format!("^(?:{suffix})"))?),
            })
        } else {
            Ok(Self {
                base: compile(pattern)?,
                excluded: None,
            })
        }
    }
    fn is_match(&self, value: &str) -> bool {
        self.base.find_iter(value).any(|m| {
            self.excluded
                .as_ref()
                .is_none_or(|r| !r.is_match(&value[m.end()..]))
        })
    }
}
#[derive(Deserialize)]
struct Probe {
    id: String,
    text: String,
}
#[derive(Deserialize)]
struct Class {
    id: String,
    lab: String,
    lab_name: String,
    aliases: Vec<String>,
    counts: Vec<i64>,
    #[serde(default)]
    alternatives: BTreeMap<String, Vec<i64>>,
}
#[derive(Deserialize)]
struct ApiModel {
    pattern: String,
    vendor: String,
    class: Option<String>,
}
#[derive(Deserialize)]
struct Wrapper {
    prefix: String,
    suffix: String,
}
#[derive(Deserialize)]
struct RawBank {
    schema: String,
    probes: Vec<Probe>,
    classes: Vec<Class>,
    api_models: Vec<ApiModel>,
    wrapper: Wrapper,
}
pub(super) struct Bank {
    raw: RawBank,
    values: Vec<Vec<Vec<i64>>>,
    scales: Vec<f64>,
    patterns: Vec<Pattern>,
    aliases: Vec<Vec<Pattern>>,
    roots: Vec<OnceLock<Vec<Vec<f64>>>>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Observation {
    pub probe: Option<usize>,
    pub tokens: i64,
}
#[derive(Clone, Serialize)]
pub(super) struct Score {
    pub id: String,
    pub exact: f64,
    pub related: f64,
    matched: usize,
    compared: usize,
    overhead: i64,
}
#[derive(Clone, Serialize)]
pub(super) struct Posterior {
    pub classes: Vec<Score>,
    pub unknown: f64,
    pub alien: f64,
    pub answered: usize,
    labs: Vec<Value>,
}
fn logsum(values: &[f64]) -> f64 {
    let top = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    top + values.iter().map(|v| (v - top).exp()).sum::<f64>().ln()
}
fn geometric(d: i64, p: f64) -> f64 {
    ((1.0 - p) / (1.0 + p)).ln() + (d.abs() as f64) * p.ln()
}
fn residual(d: i64, rate: f64) -> f64 {
    logsum(&[
        (1.0 - rate).ln() + geometric(d, 0.02),
        rate.ln() + geometric(d, 0.85),
        1e-6_f64.ln(),
    ])
}
fn nearest(delta: i64, values: &[i64]) -> i64 {
    values
        .iter()
        .map(|v| delta - v)
        .min_by_key(|d| d.abs())
        .unwrap_or(delta)
}
impl Bank {
    pub fn from_value(value: Value) -> Result<Self> {
        let raw: RawBank =
            serde_json::from_value(value).map_err(|_| failure("分词器库格式无效"))?;
        if raw.schema != "tokenizer-bank-v1"
            || !(1..=256).contains(&raw.probes.len())
            || !(2..=256).contains(&raw.classes.len())
        {
            return Err(failure("分词器库结构不兼容"));
        }
        let mut ids = HashSet::new();
        let mut probes = HashSet::new();
        if raw
            .probes
            .iter()
            .any(|p| !probes.insert(&p.id) || p.text.len() > 16_384)
        {
            return Err(failure("分词器探测文本无效"));
        }
        for c in &raw.classes {
            if !ids.insert(c.id.clone())
                || c.counts.len() != raw.probes.len()
                || c.counts.iter().any(|n| n.abs() > 4096)
                || c.alternatives.iter().any(|(j, v)| {
                    j.parse::<usize>()
                        .ok()
                        .is_none_or(|j| j >= raw.probes.len())
                        || v.is_empty()
                        || v.iter().any(|n| n.abs() > 4096)
                })
            {
                return Err(failure("分词器计数表无效"));
            }
        }
        if raw
            .api_models
            .iter()
            .any(|m| m.class.as_ref().is_some_and(|c| !ids.contains(c)))
        {
            return Err(failure("分词器型号映射不匹配"));
        }
        let compile = Pattern::new;
        let patterns = raw
            .api_models
            .iter()
            .map(|m| compile(&m.pattern))
            .collect::<Result<_>>()?;
        let aliases = raw
            .classes
            .iter()
            .map(|c| {
                c.aliases
                    .iter()
                    .map(|p| compile(p))
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<_>>()?;
        let values = raw
            .classes
            .iter()
            .map(|c| {
                c.counts
                    .iter()
                    .enumerate()
                    .map(|(j, n)| {
                        c.alternatives
                            .get(&j.to_string())
                            .cloned()
                            .unwrap_or_else(|| vec![*n])
                    })
                    .collect()
            })
            .collect();
        let scales = (0..raw.probes.len())
            .map(|j| {
                let mut counts: Vec<_> = raw.classes.iter().map(|c| c.counts[j]).collect();
                counts.sort();
                (-1.0 / (1.5_f64.max(0.15 * counts[counts.len() / 2] as f64))).exp()
            })
            .collect();
        let roots = (0..raw.probes.len()).map(|_| OnceLock::new()).collect();
        Ok(Self {
            raw,
            values,
            scales,
            patterns,
            aliases,
            roots,
        })
    }
    fn evidence(
        &self,
        rows: &[Observation],
        c: usize,
        grid: &[(f64, f64)],
        baseline: i64,
    ) -> (f64, i64) {
        let mut terms = Vec::new();
        let mut best = (f64::NEG_INFINITY, baseline);
        for offset in baseline - 3..=baseline + 3 {
            for &(rate, weight) in grid {
                let log = rows
                    .iter()
                    .map(|row| {
                        residual(
                            row.probe.map_or(row.tokens - offset, |j| {
                                nearest(row.tokens - offset, &self.values[c][j])
                            }),
                            if row.probe.is_none() { 0.02 } else { rate },
                        )
                    })
                    .sum::<f64>();
                terms.push(-7.0_f64.ln() + weight.ln() + log);
                if log > best.0 {
                    best = (log, offset);
                }
            }
        }
        (logsum(&terms), best.1)
    }
    pub fn posterior(&self, rows: &[Observation]) -> Option<Posterior> {
        let baseline = rows.iter().find(|r| r.probe.is_none())?.tokens;
        let n = self.raw.classes.len();
        let known: Vec<_> = (0..n)
            .map(|c| self.evidence(rows, c, &[(0.02, 0.4), (0.06, 0.4), (0.15, 0.2)], baseline))
            .collect();
        let related: Vec<_> = (0..n)
            .map(|c| self.evidence(rows, c, &[(0.2, 0.3), (0.4, 0.4), (0.7, 0.3)], baseline))
            .collect();
        let alien = 0.1_f64.ln()
            + logsum(
                &(baseline - 3..=baseline + 3)
                    .map(|offset| {
                        -7.0_f64.ln()
                            + rows
                                .iter()
                                .map(|r| {
                                    r.probe.map_or_else(
                                        || residual(r.tokens - offset, 0.02),
                                        |j| {
                                            let density = (0..n)
                                                .map(|c| {
                                                    geometric(
                                                        nearest(
                                                            r.tokens - offset,
                                                            &self.values[c][j],
                                                        ),
                                                        self.scales[j],
                                                    )
                                                    .exp()
                                                })
                                                .sum::<f64>()
                                                / n as f64;
                                            (density + 1e-6).ln()
                                        },
                                    )
                                })
                                .sum::<f64>()
                    })
                    .collect::<Vec<_>>(),
            );
        let k: Vec<_> = known.iter().map(|v| (0.8 / n as f64).ln() + v.0).collect();
        let r: Vec<_> = related
            .iter()
            .map(|v| (0.1 / n as f64).ln() + v.0)
            .collect();
        let norm = logsum(
            &k.iter()
                .chain(&r)
                .copied()
                .chain([alien])
                .collect::<Vec<_>>(),
        );
        let answered = rows.iter().filter(|r| r.probe.is_some()).count();
        let mut labs: Vec<Value> = Vec::new();
        let mut classes: Vec<_> = self
            .raw
            .classes
            .iter()
            .enumerate()
            .map(|(c, item)| {
                let exact = (k[c] - norm).exp();
                let related = (r[c] - norm).exp();
                if let Some(l) = labs.iter_mut().find(|l| l["lab"] == item.lab) {
                    l["total"] = json!(l["total"].as_f64().unwrap() + exact + related);
                } else {
                    labs.push(
                        json!({"lab":item.lab,"lab_name":item.lab_name,"total":exact+related}),
                    );
                }
                Score {
                    id: item.id.clone(),
                    exact,
                    related,
                    compared: answered,
                    matched: rows
                        .iter()
                        .filter(|r| {
                            r.probe.is_some_and(|j| {
                                nearest(r.tokens - known[c].1, &self.values[c][j]) == 0
                            })
                        })
                        .count(),
                    overhead: known[c].1,
                }
            })
            .collect();
        classes.sort_by(|a, b| (b.exact + b.related).total_cmp(&(a.exact + a.related)));
        labs.sort_by(|a, b| {
            b["total"]
                .as_f64()
                .unwrap()
                .total_cmp(&a["total"].as_f64().unwrap())
        });
        let alien = (alien - norm).exp();
        let unknown = alien + classes.iter().map(|c| c.related).sum::<f64>();
        Some(Posterior {
            classes,
            alien,
            unknown,
            answered,
            labs,
        })
    }
    fn roots(&self, j: usize) -> &Vec<Vec<f64>> {
        self.roots[j].get_or_init(|| {
            let values: Vec<_> = self.values.iter().map(|c| c[j][0]).collect();
            let lo = *values.iter().min().unwrap() - 24;
            let hi = *values.iter().max().unwrap() + 24;
            (0..values.len() * 2 + 1)
                .map(|a| {
                    let raw: Vec<_> = (lo..=hi)
                        .map(|y| {
                            if a == values.len() * 2 {
                                values
                                    .iter()
                                    .map(|v| geometric(y - v, self.scales[j]).exp())
                                    .sum()
                            } else {
                                residual(
                                    (y - values[a / 2]).abs().min(2047),
                                    if a % 2 == 0 { 0.06 } else { 0.4 },
                                )
                                .exp()
                            }
                        })
                        .collect();
                    let total = raw.iter().sum::<f64>();
                    raw.into_iter().map(|v| (v / total).sqrt()).collect()
                })
                .collect()
        })
    }
    pub fn next(&self, posterior: Option<&Posterior>, asked: &HashSet<usize>) -> Option<usize> {
        // (predictive-distribution index, hypothesis group, posterior weight)
        let mut atoms = Vec::new();
        if let Some(p) = posterior {
            for score in p.classes.iter().take(24) {
                let c = self
                    .raw
                    .classes
                    .iter()
                    .position(|c| c.id == score.id)
                    .unwrap();
                atoms.push((2 * c, Some(c), score.exact));
                if score.related > 0.0 {
                    atoms.push((2 * c + 1, None, score.related));
                }
            }
            if p.alien > 0.0 {
                atoms.push((2 * self.raw.classes.len(), None, p.alien));
            }
        } else {
            for c in 0..self.raw.classes.len() {
                atoms.push((2 * c, Some(c), 1.0));
            }
        }
        let total = atoms
            .iter()
            .map(|a| a.2)
            .sum::<f64>()
            .max(f64::MIN_POSITIVE);
        let mut pairs = Vec::new();
        for a in 0..atoms.len() {
            for b in a + 1..atoms.len() {
                if atoms[a].1 != atoms[b].1 {
                    pairs.push((
                        atoms[a].0,
                        atoms[b].0,
                        (atoms[a].2 * atoms[b].2).sqrt() / total,
                    ));
                }
            }
        }
        let mut best = None;
        for j in 0..self.raw.probes.len() {
            if asked.contains(&j) {
                continue;
            }
            let roots = self.roots(j);
            let gain = pairs
                .iter()
                .map(|&(a, b, w)| {
                    w * (1.0
                        - roots[a]
                            .iter()
                            .zip(&roots[b])
                            .map(|(x, y)| x * y)
                            .sum::<f64>())
                })
                .sum::<f64>();
            if best.is_none_or(|(_, old)| gain > old) {
                best = Some((j, gain));
            }
        }
        best.filter(|(_, gain)| *gain >= 1e-4).map(|(j, _)| j)
    }
    pub fn done(p: &Posterior) -> bool {
        p.answered >= 20 || p.answered >= 4 && p.classes[0].exact.max(p.unknown) >= 0.99
    }
    pub fn verdict(&self, p: &Posterior, model: &str) -> Value {
        let exact = p
            .classes
            .iter()
            .reduce(|a, b| if b.exact > a.exact { b } else { a })
            .unwrap();
        let related = p
            .classes
            .iter()
            .reduce(|a, b| if b.related > a.related { b } else { a })
            .unwrap();
        let (kind, top, confidence) = if exact.exact >= p.unknown {
            ("exact", exact, exact.exact)
        } else if related.related >= p.alien {
            ("related", related, related.related)
        } else {
            ("unknown", &p.classes[0], p.unknown)
        };
        let model = model.trim().to_lowercase();
        let model = model
            .rsplit('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        let expected = if let Some(i) = self.patterns.iter().position(|r| r.is_match(model)) {
            let api = &self.raw.api_models[i];
            Some((
                api.class.iter().cloned().collect::<Vec<_>>(),
                api.vendor.clone(),
                true,
            ))
        } else {
            let ids: Vec<_> = self
                .aliases
                .iter()
                .enumerate()
                .filter(|(_, patterns)| patterns.iter().any(|r| r.is_match(model)))
                .map(|(i, _)| i)
                .collect();
            ids.first().map(|i| {
                (
                    ids.iter()
                        .map(|i| self.raw.classes[*i].id.clone())
                        .collect(),
                    self.raw.classes[*i].lab_name.clone(),
                    false,
                )
            })
        };
        let claim=expected.map(|(ids,vendor,first)| {
            let probability=if ids.is_empty(){p.unknown}else{p.classes.iter().filter(|c|ids.contains(&c.id)).map(|c|c.exact+if first{0.0}else{c.related}).sum()};
            json!({"expected":ids,"vendor":vendor,"firstParty":first,"probability":probability,"status":if probability>=0.9{"consistent"}else if probability<=0.1{"inconsistent"}else{"uncertain"}})
        });
        json!({"kind":kind,"top":top,"confidence":confidence,"claim":claim})
    }
    fn prompt(&self, j: Option<usize>) -> String {
        format!(
            "{}{}{}",
            self.raw.wrapper.prefix,
            j.map_or("", |j| self.raw.probes[j].text.as_str()),
            self.raw.wrapper.suffix
        )
    }
}
impl Service {
    pub(super) async fn run_tokenizer(&self, id: &str, context: &Context) -> Result<()> {
        let resource = context
            .lm
            .as_ref()
            .ok_or_else(|| failure("分词器库不可用"))?
            .clone();
        let target = fingerprint::target(context)?;
        let client = target
            .client()
            .map_err(|_| failure("分词器连接初始化失败"))?;
        let mut rows = Vec::new();
        let mut asked = HashSet::new();
        let mut steps = Vec::new();
        let mut posterior = None;
        let mut next: Option<usize> = None;
        let mut final_baseline = false;
        loop {
            self.gateway(context.target.client_id)
                .verify_test_version(&context.target.provider_id, &context.target.version)?;
            self.gateway(context.target.client_id)
                .test_endpoint(&context.target.provider_id, Some(&context.target.base_url))?;
            let probe = next.map(|j| resource.tokenizer.raw.probes[j].id.clone());
            steps.push(json!({"probe":probe,"state":"requesting"}));
            self.update(id, |job, _| {
                job.tokenizer_result = Some(json!({"status":"running","steps":steps}))
            })?;
            let mut body = target
                .lm_payload(&resource.tokenizer.prompt(next))
                .map_err(|_| failure("分词器请求构造失败"))?;
            if target.protocol == "messages" {
                body["max_tokens"] = 16.into();
            } else if target.protocol == "chat_completions" {
                body["stream_options"] = json!({"include_usage":true});
            }
            let mut diag = json!({});
            let result = tokio::time::timeout(
                Duration::from_secs(90),
                fingerprint_stream::execute_body(&target, &body, 0, &client, None, &mut diag, true),
            )
            .await
            .unwrap_or_else(|_| Err(fingerprint_stream::Failure::new("timeout", false)));
            let usage = result
                .as_ref()
                .ok()
                .and_then(|_| diag["input_tokens"].as_i64());
            let Some(tokens) = usage else {
                let error = result.err().map_or(
                    "上游未返回输入用量，分词器探测已停止".to_owned(),
                    |e| fingerprint::error_message(e.code).into(),
                );
                *steps.last_mut().unwrap() = json!({"probe":probe,"state":"failed","error":error});
                self.update(id,|job,_|job.tokenizer_result=Some(json!({"status":"failed","steps":steps,"error":error,"verdict":posterior.as_ref().filter(|p:&&Posterior|p.answered>0).map(|p|resource.tokenizer.verdict(p,&target.model))})))?;
                return Ok(());
            };
            *steps.last_mut().unwrap() = json!({"probe":probe,"state":"done","tokens":tokens,"responseModel":result.ok().and_then(|r|r.1)});
            rows.push(Observation {
                probe: next,
                tokens,
            });
            if let Some(j) = next {
                asked.insert(j);
            }
            let resource2 = resource.clone();
            let observations = rows.clone();
            let seen = asked.clone();
            let (p, chosen) = tokio::task::spawn_blocking(move || {
                let bank = &resource2.tokenizer;
                let p = bank.posterior(&observations);
                let chosen = if p.as_ref().is_some_and(Bank::done) {
                    None
                } else {
                    bank.next(p.as_ref(), &seen)
                };
                (p, chosen)
            })
            .await
            .map_err(|_| failure("分词器评分失败"))?;
            posterior = p;
            if final_baseline {
                break;
            }
            next = chosen;
            if next.is_none() {
                final_baseline = true;
            }
        }
        let verdict = posterior
            .as_ref()
            .filter(|p| p.answered > 0)
            .map(|p| resource.tokenizer.verdict(p, &target.model));
        let baseline: Vec<_> = rows
            .iter()
            .filter(|r| r.probe.is_none())
            .map(|r| r.tokens)
            .collect();
        self.update(id,|job,_|job.tokenizer_result=Some(json!({"status":"completed","steps":steps,"verdict":verdict,"baselineDrift":baseline.first()!=baseline.last(),"posterior":posterior})))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serial_planning_and_posteriors_match_fixed_upstream() {
        let (_dir, service) = super::super::tests::fixture();
        let resource = service.lm_resource().unwrap();
        let bank = &resource.tokenizer;
        let fixture: Value = serde_json::from_str(include_str!("fixtures/tokenizer.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            for round in case["rounds"].as_array().unwrap() {
                let rows: Vec<_> = round["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| Observation {
                        probe: r["probe"]
                            .as_str()
                            .map(|id| bank.raw.probes.iter().position(|p| p.id == id).unwrap()),
                        tokens: r["tokens"].as_i64().unwrap(),
                    })
                    .collect();
                let posterior = bank.posterior(&rows).unwrap();
                let expected = &round["posterior"];
                assert!((posterior.unknown - expected["unknown"].as_f64().unwrap()).abs() < 1e-10);
                for a in &posterior.classes {
                    let b = expected["classes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|b| b["id"] == a.id)
                        .unwrap();
                    assert!((a.exact - b["exact"].as_f64().unwrap()).abs() < 1e-10);
                    assert!((a.related - b["related"].as_f64().unwrap()).abs() < 1e-10);
                    assert_eq!(a.matched as u64, b["matched"].as_u64().unwrap());
                }
                let next = bank.next(
                    Some(&posterior),
                    &rows.iter().filter_map(|r| r.probe).collect(),
                );
                assert_eq!(
                    next.map(|j| bank.raw.probes[j].id.as_str()),
                    round["next"].as_str()
                );
                assert_eq!(Bank::done(&posterior), round["done"]);
                let verdict = bank.verdict(&posterior, "deepseek-r1");
                assert_eq!(verdict["kind"], round["verdict"]["kind"]);
                assert_eq!(
                    verdict["claim"]["status"],
                    round["verdict"]["claim"]["status"]
                );
            }
        }
        assert!(bank.posterior(&[]).is_none());
        assert!(Pattern::new("^deepseek-r1(?!-distill)")
            .unwrap()
            .is_match("deepseek-r1"));
        assert!(!Pattern::new("^deepseek-r1(?!-distill)")
            .unwrap()
            .is_match("deepseek-r1-distill"));
    }
}
