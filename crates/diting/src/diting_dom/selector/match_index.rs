//! The rule-hash match index: per-rule match sets for `compute_styles`,
//! built by bucketing rules on their rightmost compound key and probing
//! elements against the plausible buckets (batch 191), plus the
//! dirty-stamp incremental sync that keeps those sets coherent across DOM
//! mutations (batches 191/199, #111/#123). Split out of `selector.rs` in
//! batch 202 to ride under the layering audit's god-file cap — pure index
//! and sync logic; the parse/adapter face stays in the parent module.

use std::collections::{HashMap, HashSet};

use selectors::context::QuirksMode;
use selectors::matching::{MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags};
use selectors::parser::{self, SelectorList};

use super::{parse_selector, DomElement, DitingSelector, PseudoKind};
use crate::diting_dom::tree::{DomTree, NodeId};

impl DomTree {
    /// Build [`RuleMatchSets`] for one stylesheet's rule selectors: the
    /// per-rule document match sets `compute_styles` needs, without one
    /// full-document querySelectorAll per rule.
    ///
    /// A rule selector can only match an element that carries the
    /// rightmost compound's id/class/tag, so rules are bucketed by that
    /// key and each element's keys probe only the plausible buckets; the
    /// servo matcher then confirms candidates exactly as the
    /// querySelector path would. A selector that fails to parse yields no
    /// bucket and no hits, the "never matches" outcome the old per-rule
    /// qSA error path produced.
    pub fn rule_match_sets(&self, rule_selectors: &[&str]) -> RuleMatchSets {
        // Rule matching is document-rooted (the same scope posture as a
        // document-rooted querySelectorAll): no :scope binding here. Shadow
        // descendants join the probe set so shadow `<style>` rules can match
        // shadow elements — the cascade's global-rule-pool approximation
        // (spec-scoped styles are a v3 concern). Combinators still stop at
        // the shadow root: the matcher climbs ordinary parent links, and a
        // shadow root has none.
        let mut probe_ids: Vec<NodeId> = self.descendants(self.document());
        for root in self.shadow_roots() {
            probe_ids.extend(self.descendants(root));
        }
        self.rule_match_sets_probing(rule_selectors, probe_ids)
    }

    /// Subtree-scoped variant (fabricated iframe documents): probe only the
    /// descendants of `roots`. The document-rooted probe never reaches an
    /// orphan tree, so reusing it there would silently match no rule.
    pub fn rule_match_sets_within(&self, rule_selectors: &[&str], roots: &[NodeId]) -> RuleMatchSets {
        let mut probe_ids: Vec<NodeId> = Vec::new();
        for root in roots {
            probe_ids.extend(self.descendants(*root));
        }
        self.rule_match_sets_probing(rule_selectors, probe_ids)
    }

    fn rule_match_sets_probing(
        &self,
        rule_selectors: &[&str],
        probe_ids: Vec<NodeId>,
    ) -> RuleMatchSets {
        let index = self.compile_rule_index(rule_selectors);
        let mut hits = HashMap::new();
        let mut pseudo_hits = HashMap::new();
        self.probe_elements_into(&index, probe_ids, &mut hits, &mut pseudo_hits);
        RuleMatchSets {
            hits,
            specificity: index.specificity,
            pseudo_kinds: index.pseudo_kinds,
            pseudo_hits,
        }
    }

    /// Selector-side compilation shared by the full and incremental faces:
    /// parsed selector lists, specificities, pseudo routing, and the
    /// rightmost-compound buckets. Cheap to rebuild (one parse per rule) but
    /// far from free on real sheets (~7ms for 1.7MB), so the incremental
    /// path parks it in [`MatchCacheBox`] keyed by the css bytes.
    fn compile_rule_index(&self, rule_selectors: &[&str]) -> RuleIndex {
        let mut entries: Vec<Option<SelectorList<DitingSelector>>> =
            Vec::with_capacity(rule_selectors.len());
        let mut specificity: Vec<Option<u32>> = Vec::with_capacity(rule_selectors.len());
        let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_class: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_tag: HashMap<String, Vec<usize>> = HashMap::new();
        // Rules where no sub-selector has an id/class/tag key (universal,
        // attr-only, pseudo-only compounds) stay candidates for every
        // element.
        let mut unkeyed: Vec<usize> = Vec::new();
        // In quirks mode class/id matching is ASCII-case-insensitive, so a
        // probe must reach rules keyed in the other case: bucket and probe
        // both spellings there. Over-bucketing is always safe (the matcher
        // has the final word); a bucket the probe cannot reach never is.
        let quirks = self.selector_quirks_mode() == QuirksMode::Quirks;
        let mut pseudo_kinds: HashMap<usize, PseudoKind> = HashMap::new();
        let mut bears_has = false;
        let mut has_subject_ids: HashSet<String> = HashSet::new();
        let mut has_subject_classes: HashSet<String> = HashSet::new();
        let mut has_subject_tags: HashSet<String> = HashSet::new();
        let mut has_subject_unkeyed = false;

        for (ri, selector) in rule_selectors.iter().enumerate() {
            // A trailing pseudo-element suffix routes the rule to the
            // pseudo cascade instead of the normal one. Only a suffix at
            // the very end of a comma-free selector is recognized; anything
            // else (`a::before b`, `a::before:hover`, comma lists) parses
            // with the suffix attached and stays dead — exactly what
            // MatchingMode::Normal did to it before pseudo support.
            let mut base_selector = *selector;
            let sel_trim = selector.trim();
            if !sel_trim.contains(',') {
                let lower = sel_trim.to_ascii_lowercase();
                let stripped = lower
                    .strip_suffix("::before")
                    .map(|rest| (rest.len(), PseudoKind::Before))
                    .or_else(|| {
                        lower
                            .strip_suffix("::after")
                            .map(|rest| (rest.len(), PseudoKind::After))
                    })
                    .or_else(|| {
                        lower
                            .strip_suffix(":before")
                            .map(|rest| (rest.len(), PseudoKind::Before))
                    })
                    .or_else(|| {
                        lower
                            .strip_suffix(":after")
                            .map(|rest| (rest.len(), PseudoKind::After))
                    });
                // to_ascii_lowercase is byte-for-byte, so slicing the
                // original by the lowered length keeps the author's case.
                if let Some((base_len, kind)) = stripped {
                    if let Some(base) = sel_trim.get(..base_len) {
                        base_selector = base;
                        pseudo_kinds.insert(ri, kind);
                    }
                }
            }
            let Ok(list) = parse_selector(base_selector) else {
                entries.push(None);
                specificity.push(None);
                continue;
            };
            specificity.push(list.slice().first().map(|s| s.specificity()));
            // A comma list matches if ANY sub-selector matches, so every
            // sub-selector contributes its own bucket entry.
            let mut bucketed = false;
            for sel in list.slice() {
                match rightmost_key(sel) {
                    Some(RuleKey::Id(id)) => {
                        by_id.entry(id.clone()).or_default().push(ri);
                        if quirks {
                            by_id.entry(id.to_ascii_lowercase()).or_default().push(ri);
                        }
                        bucketed = true;
                    }
                    Some(RuleKey::Class(class)) => {
                        by_class.entry(class.clone()).or_default().push(ri);
                        if quirks {
                            by_class
                                .entry(class.to_ascii_lowercase())
                                .or_default()
                                .push(ri);
                        }
                        bucketed = true;
                    }
                    Some(RuleKey::Tag(tag)) => {
                        by_tag.entry(tag).or_default().push(ri);
                        bucketed = true;
                    }
                    None => {}
                }
            }
            if !bucketed {
                unkeyed.push(ri);
            }
            if list.slice().iter().any(selector_bears_has) {
                bears_has = true;
                // Subject keys for the transitive re-probe: every
                // sub-selector's rightmost compound, because a comma list
                // matches when any sub-selector does. A keyless subject
                // compound (`:has(a) *`) leaves every element a candidate,
                // which the candidate gate converts to a full rebuild.
                for sel in list.slice() {
                    match rightmost_key(sel) {
                        Some(RuleKey::Id(id)) => {
                            has_subject_ids.insert(id.clone());
                            if quirks {
                                has_subject_ids.insert(id.to_ascii_lowercase());
                            }
                        }
                        Some(RuleKey::Class(class)) => {
                            has_subject_classes.insert(class.clone());
                            if quirks {
                                has_subject_classes.insert(class.to_ascii_lowercase());
                            }
                        }
                        Some(RuleKey::Tag(tag)) => {
                            has_subject_tags.insert(tag);
                        }
                        None => has_subject_unkeyed = true,
                    }
                }
            }
            entries.push(Some(list));
        }
        RuleIndex {
            entries,
            specificity,
            by_id,
            by_class,
            by_tag,
            unkeyed,
            pseudo_kinds,
            quirks,
            bears_has,
            has_subject_ids,
            has_subject_classes,
            has_subject_tags,
            has_subject_unkeyed,
        }
    }

    /// Probe elements against the compiled index, appending matches to the
    /// (possibly pre-populated, possibly purge-filtered) hit maps. Shared by
    /// the full build and the incremental sync so the two cannot drift.
    fn probe_elements_into(
        &self,
        index: &RuleIndex,
        probe_ids: impl IntoIterator<Item = NodeId>,
        hits: &mut HashMap<usize, Vec<usize>>,
        pseudo_hits: &mut HashMap<usize, Vec<usize>>,
    ) {
        let mut caches = selectors::context::SelectorCaches::default();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            self.selector_quirks_mode(),
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        let mut candidates: Vec<usize> = Vec::new();
        for desc_id in probe_ids {
            let Some((local, id, class)) = self
                .with_node(desc_id, |n| {
                    let name = n.as_element()?;
                    Some((
                        name.local.as_ref().to_string(),
                        n.get_attribute("id").map(|s| s.to_string()),
                        n.get_attribute("class").map(|s| s.to_string()),
                    ))
                })
                .flatten()
            else {
                continue;
            };
            candidates.clear();
            candidates.extend_from_slice(&index.unkeyed);
            let mut probe = |bucket: &HashMap<String, Vec<usize>>, key: &str| {
                if let Some(rules) = bucket.get(key) {
                    candidates.extend_from_slice(rules);
                }
            };
            // Tag selectors bucket under the parser's lower_name; probing
            // the element's lowercased local name is the shared spelling.
            // Foreign-element tags keep camelCase local names in the DOM
            // (SVG clipPath and friends), and lowercasing the probe covers
            // them too.
            probe(&index.by_tag, &local.to_ascii_lowercase());
            if let Some(id) = &id {
                probe(&index.by_id, id);
                if index.quirks {
                    probe(&index.by_id, &id.to_ascii_lowercase());
                }
            }
            if let Some(class) = &class {
                for c in class.split_whitespace() {
                    probe(&index.by_class, c);
                    if index.quirks {
                        probe(&index.by_class, &c.to_ascii_lowercase());
                    }
                }
            }
            candidates.sort_unstable();
            candidates.dedup();
            let element = DomElement::new(self, desc_id);
            for ri in candidates.drain(..) {
                let Some(list) = index.entries[ri].as_ref() else { continue };
                if selectors::matching::matches_selector_list(list, &element, &mut context) {
                    if index.pseudo_kinds.contains_key(&ri) {
                        pseudo_hits.entry(ri).or_default().push(desc_id.index());
                    } else {
                        hits.entry(ri).or_default().push(desc_id.index());
                    }
                }
            }
        }
        // The walk is document order, not node-index order; the cascade
        // binary-searches these, so each rule's hit set ends ascending.
        for rule_hits in hits.values_mut().chain(pseudo_hits.values_mut()) {
            rule_hits.sort_unstable();
        }
    }

    /// The full probe set: document descendants plus every shadow tree's,
    /// the same population `rule_match_sets` covers.
    fn match_probe_all_ids(&self) -> Vec<NodeId> {
        let mut probe_ids: Vec<NodeId> = self.descendants(self.document());
        for root in self.shadow_roots() {
            probe_ids.extend(self.descendants(root));
        }
        probe_ids
    }

    /// Whether `nid` is reachable from the document or a shadow root — the
    /// population a fresh full match would probe. Detached subtrees (removed
    /// but not freed, or awaiting re-insertion) must not contribute hits a
    /// full rebuild would not produce.
    fn match_probe_attached(&self, nid: NodeId) -> bool {
        let inner = self.borrow_inner();
        let mut current = Some(nid);
        for _ in 0..=inner.nodes.len() {
            let Some(c) = current else { return false };
            if c == inner.document || inner.shadow_roots.contains_key(&c) {
                return true;
            }
            current = inner
                .nodes
                .get(c.index())
                .and_then(|n| n.as_ref())
                .and_then(|n| n.parent);
        }
        false
    }

    /// Incremental face for repeated layout runs against the same stylesheet
    /// (#111): the hit sets persist in the tree's [`MatchCacheBox`] keyed by
    /// the css bytes, and DOM mutations recorded in [`MatchDirty`] since the
    /// last sync only re-probe the elements they could have affected. The
    /// result is bit-for-bit what a fresh full match produces — the
    /// correctness argument is stamp completeness: an insert/re-parent only
    /// changes matches inside the moved subtree (ancestor chains) plus the
    /// two child lists it touches (sibling combinators, :nth-child) plus the
    /// parent chain (:has anchoring); an attribute write only changes the
    /// element, its subtree, its siblings, and its ancestor chain. Anything
    /// the registry cannot express falls back to a full rebuild.
    pub fn rule_match_sets_incremental(&self, rule_selectors: &[&str], css_key: u64) -> RuleMatchSets {
        let trace = std::env::var("AGINXBROWSER_MATCH_TRACE").is_ok();
        let dirty = self.take_match_dirty();
        let mut cache = match self.take_match_cache(css_key) {
            Some(cache) => cache,
            None => {
                // First run, or the css changed: full build against fresh
                // buckets, then park the result for the next sync.
                let index = self.compile_rule_index(rule_selectors);
                let mut hits = HashMap::new();
                let mut pseudo_hits = HashMap::new();
                self.probe_elements_into(
                    &index,
                    self.match_probe_all_ids(),
                    &mut hits,
                    &mut pseudo_hits,
                );
                let sets = RuleMatchSets {
                    hits,
                    specificity: index.specificity.clone(),
                    pseudo_kinds: index.pseudo_kinds.clone(),
                    pseudo_hits,
                };
                self.set_match_cache(MatchCacheBox {
                    key: css_key,
                    index,
                    sets: sets.clone(),
                });
                if trace {
                    eprintln!(
                        "[match-trace] FULL-BUILD key={css_key:#x} rules={} elements={}",
                        rule_selectors.len(),
                        sets.hits.values().map(Vec::len).sum::<usize>(),
                    );
                }
                return sets;
            }
        };

        let rebuild = |tree: &Self, cache: MatchCacheBox| -> RuleMatchSets {
            let mut hits = HashMap::new();
            let mut pseudo_hits = HashMap::new();
            tree.probe_elements_into(
                &cache.index,
                tree.match_probe_all_ids(),
                &mut hits,
                &mut pseudo_hits,
            );
            let sets = RuleMatchSets {
                hits,
                specificity: cache.index.specificity.clone(),
                pseudo_kinds: cache.index.pseudo_kinds.clone(),
                pseudo_hits,
            };
            tree.set_match_cache(MatchCacheBox { sets: sets.clone(), ..cache });
            sets
        };

        if dirty.full {
            if trace {
                eprintln!("[match-trace] REBUILD(dirty.full) key={css_key:#x}");
            }
            return rebuild(self, cache);
        }

        let slots = self.node_slot_count();
        let mut seen = vec![false; slots];
        let mut mark = |nid: NodeId| {
            let i = nid.index();
            if i < slots && !seen[i] {
                seen[i] = true;
                true
            } else {
                false
            }
        };
        // Fast gate: the stamp count is a lower bound on the candidate
        // count (every root contributes at least itself), so a mass
        // mutation (initial parse, big innerHTML) skips candidate
        // resolution entirely and rebuilds — the pre-incremental cost.
        let stamp_count = dirty.roots.len()
            + dirty.purge_roots.len()
            + dirty.sibling_scopes.len();
        if stamp_count.saturating_mul(4) > slots {
            if trace {
                eprintln!(
                    "[match-trace] REBUILD(stamp-gate) key={css_key:#x} stamps={stamp_count} slots={slots}"
                );
            }
            return rebuild(self, cache);
        }

        // Resolve the dirty stamps into a deduped candidate set. Slots is a
        // safe index ceiling: the arena never shrinks, and freed slots that
        // come back carry their own insert stamps.
        let mut candidates: Vec<NodeId> = Vec::new();
        for root in dirty.roots.iter().copied().chain(dirty.purge_roots.iter().copied()) {
            if mark(root) {
                candidates.push(root);
            }
            for d in self.descendants(root) {
                if mark(d) {
                    candidates.push(d);
                }
            }
        }
        for parent in &dirty.sibling_scopes {
            // The children re-probe wholesale; the parent chain re-probes
            // one element at a time (:has() anchoring).
            let mut chain = Some(*parent);
            for _ in 0..=slots {
                let Some(c) = chain else { break };
                if mark(c) {
                    candidates.push(c);
                }
                // #123: a :has()-bearing rule matches transitive to the
                // subject's attached descendants — flipping the chain
                // node's :has() state flips them, and they are nowhere
                // near the mutation. Only elements carrying a :has rule's
                // rightmost key can be such subjects, so the subtree walk
                // marks those alone (an unkeyed :has rule degenerates to
                // marking the whole subtree, and the candidate gate turns
                // that into a full rebuild).
                if cache.index.bears_has {
                    for d in self.descendants(c) {
                        let meta = self.with_node(d, |n| {
                            n.as_element().map(|e| {
                                (
                                    e.local.as_ref().to_ascii_lowercase(),
                                    n.get_attribute("id").map(|s| s.to_string()),
                                    n.get_attribute("class").map(|s| s.to_string()),
                                )
                            })
                        });
                        if !subtree_node_is_has_subject(meta.flatten(), &cache.index) {
                            continue;
                        }
                        if mark(d) {
                            candidates.push(d);
                        }
                    }
                }
                chain = self
                    .with_node(c, |n| n.parent)
                    .flatten();
            }
            for c in self.children(*parent) {
                if mark(c) {
                    candidates.push(c);
                }
            }
        }
        // A mass mutation (initial parse, big innerHTML) costs more to
        // resolve than a full rebuild — same as the pre-incremental world.
        if candidates.len() * 4 > slots {
            if trace {
                eprintln!(
                    "[match-trace] REBUILD(candidate-gate) key={css_key:#x} candidates={} slots={slots}",
                    candidates.len()
                );
            }
            return rebuild(self, cache);
        }

        // Purge every candidate index from the hit sets, then re-probe the
        // attached ones. Detached subtree members stay purged (a full
        // rebuild probes only document+shadow descendants), and a freed
        // slot's stale hits die here too.
        let candidates_len = candidates.len();
        for rule_hits in cache.sets.hits.values_mut().chain(cache.sets.pseudo_hits.values_mut()) {
            rule_hits.retain(|&i| !(i < slots && seen[i]));
        }
        // A fresh build only keys rules that matched at least one element;
        // drop the vectors the purge emptied so the shapes stay identical.
        cache.sets.hits.retain(|_, v| !v.is_empty());
        cache.sets.pseudo_hits.retain(|_, v| !v.is_empty());
        let attached: Vec<NodeId> = candidates
            .into_iter()
            .filter(|&c| self.match_probe_attached(c))
            .collect();
        if trace {
            eprintln!(
                "[match-trace] INCREMENTAL key={css_key:#x} roots={} purge={} sib={} candidates={} attached={}",
                dirty.roots.len(),
                dirty.purge_roots.len(),
                dirty.sibling_scopes.len(),
                candidates_len,
                attached.len()
            );
        }
        self.probe_elements_into(
            &cache.index,
            attached,
            &mut cache.sets.hits,
            &mut cache.sets.pseudo_hits,
        );
        let sets = cache.sets.clone();
        self.set_match_cache(cache);
        sets
    }
}

/// True when any component of the selector tree contains a `:has()`
/// functional pseudo — including nested inside `:is`/`:where`/`:not`,
/// another `:has`'s relative selectors, `:nth-child(of …)`, `::slotted`,
/// or `:host` arguments. A `:has`-bearing rule matches TRANSITIVELY:
/// mutating a node inside the `:has` subject flips the subject's own
/// match, and a combinator to the subject's right
/// (`section:has(.leaf) .ghost`) flips subjects that are attached
/// descendants of it — nowhere near the mutation (#123). The incremental
/// sync reacts by extending the ancestor-chain re-probe to each chain
/// node's subtree when (and only when) some rule bears `:has`; sheets
/// without it keep the tight candidate set.
fn selector_bears_has(sel: &parser::Selector<DitingSelector>) -> bool {
    use parser::Component;
    fn list_bears(list: &SelectorList<DitingSelector>) -> bool {
        list.slice().iter().any(selector_bears_has)
    }
    fn component_bears(c: &Component<DitingSelector>) -> bool {
        match c {
            Component::Has(_) => true,
            Component::Is(list) | Component::Where(list) | Component::Negation(list) => {
                list_bears(list)
            }
            Component::NthOf(nth) => nth.selectors().iter().any(selector_bears_has),
            Component::Slotted(sel) => selector_bears_has(sel),
            Component::Host(Some(sel)) => selector_bears_has(sel),
            _ => false,
        }
    }
    // iter_raw_match_order, not iter: `iter()` stops at the rightmost
    // compound boundary, so a `:has` left of a combinator
    // (`section:has(.leaf) .ghost`) would be invisible to it.
    sel.iter_raw_match_order().any(component_bears)
}

/// Whether an element carrying these keys (lowercased tag, id, class
/// attribute) can be the subject of a `:has`-bearing rule in `index` —
/// the filter the subtree walk under a flipped chain node applies. Set
/// spellings mirror the compile side (both author cases in quirks mode),
/// so over-inclusion is the only failure mode the filter can have.
fn subtree_node_is_has_subject(
    meta: Option<(String, Option<String>, Option<String>)>,
    index: &RuleIndex,
) -> bool {
    let Some((tag, id, class)) = meta else {
        return false;
    };
    if index.has_subject_unkeyed {
        return true;
    }
    if index.has_subject_tags.contains(tag.as_str()) {
        return true;
    }
    if let Some(id) = id {
        if index.has_subject_ids.contains(id.as_str())
            || (index.quirks && index.has_subject_ids.contains(id.to_ascii_lowercase().as_str()))
        {
            return true;
        }
    }
    if let Some(class) = class {
        for tok in class.split_ascii_whitespace() {
            if index.has_subject_classes.contains(tok)
                || (index.quirks
                    && index.has_subject_classes.contains(tok.to_ascii_lowercase().as_str()))
            {
                return true;
            }
        }
    }
    false
}

/// The rightmost-compound key a rule selector's subject element must
/// carry, ranked by selectivity (id > class > tag). Extracted per
/// sub-selector for [`DomTree::rule_match_sets`]'s buckets; a compound
/// with none of the three keys nothing, so its rule is tested against
/// every element.
enum RuleKey {
    Tag(String),
    Class(String),
    Id(String),
}

impl RuleKey {
    fn rank(&self) -> u8 {
        match self {
            RuleKey::Tag(_) => 0,
            RuleKey::Class(_) => 1,
            RuleKey::Id(_) => 2,
        }
    }
}

/// Extract the key from one parsed selector's rightmost compound. Servo's
/// `Selector::iter()` yields components right-to-left starting at the
/// rightmost compound; the iterator stops at the compound boundary, which
/// is exactly the span that constrains the subject element (anything left
/// of a combinator describes ancestors, not the subject).
fn rightmost_key(selector: &parser::Selector<DitingSelector>) -> Option<RuleKey> {
    let mut best: Option<RuleKey> = None;
    for component in selector.iter() {
        let key = match component {
            parser::Component::ID(id) => Some(RuleKey::Id(id.0.clone())),
            parser::Component::Class(class) => Some(RuleKey::Class(class.0.clone())),
            // The DOM-side probe reaches this bucket through the element's
            // lowercased local name, so the parser's lower_name is the
            // spelling both sides agree on.
            parser::Component::LocalName(name) => Some(RuleKey::Tag(name.lower_name.0.to_string())),
            _ => None,
        };
        if let Some(key) = key {
            if best.as_ref().is_none_or(|b| key.rank() > b.rank()) {
                best = Some(key);
            }
        }
    }
    best
}

/// Chrome-style rule-hash match sets for stylesheet application, built by
/// [`DomTree::rule_match_sets`].
///
/// `compute_styles` used to precompute each rule's document match set
/// with one full-document `querySelectorAll` per rule: O(rules x docsize)
/// selector matches per layout run, re-paid on every epoch bump. On the
/// WeChat article pages (6912 rules, 483 elements, 3 MB of CSS) that
/// phase alone was ~3.7s of EVERY layout run, and page scripts'
/// write-then-read layout thrashing re-triggered it several times per
/// navigation, the engine-side root cause behind the appmsg.js
/// synchronous "dead spin" that blew the nav deadline (a V8 terminate
/// cannot land inside a long Rust phase).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleMatchSets {
    /// Rule index to matching element node indices, ascending: ready for
    /// the cascade's per-element binary-search membership test (which the
    /// old code documented but then did linearly).
    pub hits: HashMap<usize, Vec<usize>>,
    /// Rule index to the rule selector's specificity, parsed once here
    /// instead of re-parsed per matched element (the old cascade called
    /// `compile_rule_selector` per element x matched rule).
    pub specificity: Vec<Option<u32>>,
    /// Pseudo-element rules (`div::before { ... }`), keyed by rule index.
    /// A rule carries at most one kind (the suffix sits on the single
    /// selector), and its hits live in [`RuleMatchSets::pseudo_hits`] so
    /// the normal cascade never applies the base compound to real elements.
    pub pseudo_kinds: HashMap<usize, PseudoKind>,
    /// Pseudo-element rule hit sets, same shape as `hits` but keyed by the
    /// rule indexes present in `pseudo_kinds`.
    pub pseudo_hits: HashMap<usize, Vec<usize>>,
}

/// The compiled selector-side state a match run probes against: parsed
/// lists, specificities, pseudo routing, and the rightmost-compound
/// buckets (see [`DomTree::rule_match_sets`]). Lives in
/// [`MatchCacheBox`] across incremental runs keyed by the css bytes.
pub(crate) struct RuleIndex {
    entries: Vec<Option<SelectorList<DitingSelector>>>,
    specificity: Vec<Option<u32>>,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    unkeyed: Vec<usize>,
    pseudo_kinds: HashMap<usize, PseudoKind>,
    quirks: bool,
    /// Some rule bears `:has()` (see [`selector_bears_has`]): matching is
    /// transitive, so the incremental sync widens its ancestor-chain
    /// re-probe to whole subtrees. False for `:has`-free sheets — the
    /// common case pays nothing.
    bears_has: bool,
    /// Rightmost-compound keys of the `:has`-bearing rules' sub-selectors
    /// (`section:has(.leaf) .ghost` contributes class "ghost"): the only
    /// elements the subtree walk under a flipped chain node re-probes.
    /// Both author-case spellings land in the sets in quirks mode, mirroring
    /// the bucket tables.
    has_subject_ids: HashSet<String>,
    has_subject_classes: HashSet<String>,
    has_subject_tags: HashSet<String>,
    /// A `:has`-bearing rule has a keyless subject compound (`:has(a) *`):
    /// every element is a candidate, so the walk marks whole subtrees and
    /// the candidate gate turns that into a full rebuild.
    has_subject_unkeyed: bool,
}

/// The persisted match state for [`DomTree::rule_match_sets_incremental`]:
/// the compiled index and the live hit sets, valid as long as the css bytes
/// behind `key` are unchanged and every tree mutation since the last sync
/// carried a [`MatchDirty`] stamp.
pub(crate) struct MatchCacheBox {
    pub(crate) key: u64,
    index: RuleIndex,
    sets: RuleMatchSets,
}

/// Mutations the incremental matcher needs to know about, stamped by the
/// tree at every structural change and by `note_restyle` at attribute
/// writes. Consumed (emptied) by the next
/// [`DomTree::rule_match_sets_incremental`] sync.
#[derive(Default)]
pub(crate) struct MatchDirty {
    /// Roots of inserted, re-parented, or restyled subtrees: every element
    /// at and below re-probes.
    pub(crate) roots: Vec<NodeId>,
    /// Parents whose child list changed: their element children re-probe
    /// (sibling combinators and :nth-child shift), and each parent's
    /// ancestor chain re-probes individually (`:has()` anchoring — a
    /// subtree entering or leaving these descendants can flip a `:has()`
    /// compound up the chain). The chain walk happens at sync time, deduped
    /// across stamps, so a mutation stays O(1).
    pub(crate) sibling_scopes: Vec<NodeId>,
    /// Roots of detached subtrees: same treatment as `roots` — if the
    /// subtree was re-inserted elsewhere it also carries a `roots` stamp
    /// and re-probes; if it stayed detached the purge drops its hits; if
    /// the slot was freed and reused the new occupant carries its own
    /// insert stamp, so the purge never outlives its meaning.
    pub(crate) purge_roots: Vec<NodeId>,
    /// Set when a mutation path the registry cannot describe ran — the
    /// next sync rebuilds from scratch.
    pub(crate) full: bool,
}
