//! Colocated contract suites for the DOM modules — tree construction
//! invariants and selector matching, including the incremental matcher
//! differential (batch 191). The layering audit exempts files named
//! tests.rs from the god-file ratchet: colocated suites are a feature.

mod tree_tests {

    use crate::diting_dom::tree::*;
    use html5ever::{local_name, namespace_url, ns, LocalName, Namespace, QualName};

    fn element(tree: &DomTree, local: &str) -> NodeId {
        tree.new_node(NodeData::Element {
            name: QualName::new(None, ns!(html), LocalName::from(local)),
            attrs: vec![],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            live_value: None,
            live_checked: None,
        })
    }

    fn element_with_id(tree: &DomTree, local: &str, id: &str) -> NodeId {
        tree.new_node(NodeData::Element {
            name: QualName::new(None, ns!(html), LocalName::from(local)),
            attrs: vec![Attribute {
                name: QualName::new(None, Namespace::default(), LocalName::from("id")),
                value: id.into(),
            }],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            live_value: None,
            live_checked: None,
        })
    }

    #[test]
    fn test_new_tree_has_document() {
        let tree = DomTree::new();
        assert_eq!(tree.len(), 1);
        let node = tree.get_node(tree.document()).unwrap();
        assert!(node.is_document());
    }

    #[test]
    fn test_append_child() {
        let tree = DomTree::new();
        let child = tree.new_node(NodeData::Text {
            contents: "hello".into(),
        });
        let doc = tree.document();
        tree.append_child(doc, child);

        assert_eq!(tree.len(), 2);
        let doc_node = tree.get_node(doc).unwrap();
        assert_eq!(doc_node.first_child, Some(child));
        assert_eq!(doc_node.last_child, Some(child));

        let child_node = tree.get_node(child).unwrap();
        assert_eq!(child_node.parent, Some(doc));
    }

    #[test]
    fn test_multiple_children() {
        let tree = DomTree::new();
        let doc = tree.document();
        let c1 = tree.new_node(NodeData::Text { contents: "a".into() });
        let c2 = tree.new_node(NodeData::Text { contents: "b".into() });
        let c3 = tree.new_node(NodeData::Text { contents: "c".into() });
        tree.append_child(doc, c1);
        tree.append_child(doc, c2);
        tree.append_child(doc, c3);

        assert_eq!(tree.children(doc), vec![c1, c2, c3]);
    }

    #[test]
    fn test_detach() {
        let tree = DomTree::new();
        let doc = tree.document();
        let c1 = tree.new_node(NodeData::Text { contents: "a".into() });
        let c2 = tree.new_node(NodeData::Text { contents: "b".into() });
        tree.append_child(doc, c1);
        tree.append_child(doc, c2);

        tree.detach(c1);
        assert_eq!(tree.children(doc), vec![c2]);
    }

    #[test]
    fn test_insert_before() {
        let tree = DomTree::new();
        let doc = tree.document();
        let c1 = tree.new_node(NodeData::Text { contents: "a".into() });
        let c2 = tree.new_node(NodeData::Text { contents: "b".into() });
        let c3 = tree.new_node(NodeData::Text { contents: "c".into() });
        tree.append_child(doc, c1);
        tree.append_child(doc, c3);
        tree.insert_before(c3, c2);

        assert_eq!(tree.children(doc), vec![c1, c2, c3]);
    }

    #[test]
    fn test_text_content() {
        let tree = DomTree::new();
        let doc = tree.document();
        let div = tree.new_node(NodeData::Element {
            name: QualName::new(None, ns!(html), local_name!("div")),
            attrs: vec![],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            live_value: None,
            live_checked: None,
        });
        tree.append_child(doc, div);

        let t1 = tree.new_node(NodeData::Text { contents: "Hello ".into() });
        let t2 = tree.new_node(NodeData::Text { contents: "World".into() });
        tree.append_child(div, t1);
        tree.append_child(div, t2);

        assert_eq!(tree.text_content(div), "Hello World");
    }

    #[test]
    fn test_get_element_by_id() {
        let tree = DomTree::new();
        let doc = tree.document();
        let div = tree.new_node(NodeData::Element {
            name: QualName::new(None, ns!(html), local_name!("div")),
            attrs: vec![Attribute {
                name: QualName::new(None, Namespace::default(), LocalName::from("id")),
                value: "main".into(),
            }],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            live_value: None,
            live_checked: None,
        });
        tree.append_child(doc, div);

        assert_eq!(tree.get_element_by_id("main"), Some(div));
        assert_eq!(tree.get_element_by_id("nonexistent"), None);
    }

    #[test]
    fn test_reparent_cycle_is_rejected() {
        // document -> html -> body -> div. Moving an ancestor under one of its
        // own descendants would make the parent/child graph cyclic and hang
        // every later descendants() walk. Both append_child and insert_before
        // must reject it as a no-op (DOM HierarchyRequestError).
        let tree = DomTree::new();
        let doc = tree.document();
        let mk = |n: &str| {
            tree.new_node(NodeData::Element {
                name: QualName::new(None, ns!(html), LocalName::from(n)),
                attrs: vec![],
                template_contents: None,
                mathml_annotation_xml_integration_point: false,
                live_value: None,
                live_checked: None,
            })
        };
        let html = mk("html");
        let body = mk("body");
        let div = mk("div");
        tree.append_child(doc, html);
        tree.append_child(html, body);
        tree.append_child(body, div);

        let before = tree.descendants(doc).len();
        assert_eq!(before, 3);

        // append_child: html is an ancestor of div -> must be a no-op, no cycle.
        tree.append_child(div, html);
        assert_eq!(tree.descendants(doc).len(), before, "cyclic append must be a no-op");
        assert_eq!(tree.descendants(div).len(), 0, "div must stay a leaf");

        // insert_before: html is an ancestor of body (div's parent) -> no-op.
        tree.insert_before(div, html);
        assert_eq!(tree.descendants(doc).len(), before, "cyclic insert_before must be a no-op");

        // self-append / self-insert remain no-ops (existing guards).
        tree.append_child(div, div);
        tree.insert_before(div, div);
        assert_eq!(tree.descendants(doc).len(), before);
    }

    #[test]
    fn test_insert_before_previous_sibling_no_cycle() {
        // Inserting a node before its own immediate previous sibling is a no-op
        // reorder that frameworks do constantly. It used to splice
        // next_sibling = self via a prev_id captured before detach, hanging every
        // later sibling walk (this hung ebay.com). The result must stay a
        // well-formed [a, b] with no cycle.
        let tree = DomTree::new();
        let doc = tree.document();
        let mk = |n: &str| {
            tree.new_node(NodeData::Element {
                name: QualName::new(None, ns!(html), LocalName::from(n)),
                attrs: vec![],
                template_contents: None,
                mathml_annotation_xml_integration_point: false,
                live_value: None,
                live_checked: None,
            })
        };
        let parent = mk("div");
        let a = mk("a");
        let b = mk("b");
        tree.append_child(doc, parent);
        tree.append_child(parent, a);
        tree.append_child(parent, b); // parent -> [a, b]

        // a is already b's previous sibling; this reorder must not create a cycle.
        tree.insert_before(b, a);

        let kids = tree.descendants(parent);
        assert_eq!(kids, vec![a, b], "order preserved, no cycle");
    }

    #[test]
    fn test_append_text_merges() {
        let tree = DomTree::new();
        let doc = tree.document();
        tree.append_text(doc, "Hello ");
        tree.append_text(doc, "World");

        assert_eq!(tree.children(doc).len(), 1);
        assert_eq!(tree.text_content(doc), "Hello World");
    }

    #[test]
    fn test_remove_subtree() {
        let tree = DomTree::new();
        let doc = tree.document();
        let div = tree.new_node(NodeData::Element {
            name: QualName::new(None, ns!(html), local_name!("div")),
            attrs: vec![],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            live_value: None,
            live_checked: None,
        });
        tree.append_child(doc, div);
        let text = tree.new_node(NodeData::Text { contents: "hi".into() });
        tree.append_child(div, text);

        assert_eq!(tree.len(), 3);
        tree.remove(div);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn test_double_remove_does_not_double_free() {
        // Removing the same node twice must be a no-op the second time. The
        // unguarded path pushed the slot onto the free list twice, so two later
        // new_node() calls could be handed the SAME NodeId (aliasing).
        let tree = DomTree::new();
        let doc = tree.document();
        let a = tree.new_node(NodeData::Text { contents: "a".into() });
        tree.append_child(doc, a);

        tree.remove(a);
        tree.remove(a); // must not free the slot a second time

        let b = tree.new_node(NodeData::Text { contents: "b".into() });
        let c = tree.new_node(NodeData::Text { contents: "c".into() });
        assert_ne!(b, c, "double-free handed the same slot to two live nodes");
        assert_eq!(b, a, "first new node should reuse the freed slot");
    }

    #[test]
    fn test_children_and_ancestors_survive_forced_cycles() {
        // The tree guards make cycles unreachable via the public API, so force
        // corrupt pointers directly: children()/ancestors() must terminate with
        // a bounded result instead of looping forever.
        let tree = DomTree::new();
        let doc = tree.document();
        let a = tree.new_node(NodeData::Text { contents: "a".into() });
        let b = tree.new_node(NodeData::Text { contents: "b".into() });
        tree.append_child(doc, a);
        tree.append_child(doc, b);

        // Sibling cycle: a <-> b.
        tree.with_node_mut(a, |n| n.next_sibling = Some(b));
        tree.with_node_mut(b, |n| n.prev_sibling = Some(a));
        tree.with_node_mut(b, |n| n.next_sibling = Some(a));
        tree.with_node_mut(a, |n| n.prev_sibling = Some(b));
        let kids = tree.children(doc);
        assert!(kids.len() <= tree.node_slot_count() + 1, "children() ran away: {}", kids.len());

        // Parent cycle: a.parent = b, b.parent = a.
        tree.with_node_mut(a, |n| n.parent = Some(b));
        tree.with_node_mut(b, |n| n.parent = Some(a));
        let anc = tree.ancestors(a);
        assert!(anc.len() <= tree.node_slot_count() + 1, "ancestors() ran away: {}", anc.len());
    }

    #[test]
    fn test_set_attribute_matches_qualified_name() {
        // A parsed namespaced attribute stores prefix separately from the local
        // name (xlink:href -> prefix="xlink", local="href"). set_attribute must
        // match on the qualified name, or it silently pushes a duplicate.
        let tree = crate::diting_dom::tree_sink::parse_html(
            r##"<svg><use xlink:href="#icon"/></svg>"##,
        );
        let use_el = tree.query_selector("use").unwrap().unwrap();

        tree.with_node_mut(use_el, |n| n.set_attribute("xlink:href", "#other".into()));
        tree.with_node(use_el, |n| {
            let attrs = n.attrs().unwrap();
            assert_eq!(attrs.len(), 1, "qualified-name set must update in place, got {attrs:?}");
            assert_eq!(n.get_attribute("xlink:href"), Some("#other"));
        });

        // And a bare "href" must NOT match the namespaced "xlink:href".
        tree.with_node_mut(use_el, |n| n.set_attribute("href", "#plain".into()));
        tree.with_node(use_el, |n| {
            assert_eq!(n.attrs().unwrap().len(), 2);
            assert_eq!(n.get_attribute("href"), Some("#plain"));
            assert_eq!(n.get_attribute("xlink:href"), Some("#other"));
        });
    }

    #[test]
    fn test_attribute_ns_roundtrip() {
        let tree = DomTree::new();
        let doc = tree.document();
        let el = tree.new_node(NodeData::Element {
            name: QualName::new(None, ns!(html), local_name!("div")),
            attrs: vec![],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            live_value: None,
            live_checked: None,
        });
        tree.append_child(doc, el);

        tree.with_node_mut(el, |n| {
            n.set_attribute_ns("http://www.w3.org/1999/xlink", "xlink:href", "#a".into())
        });
        tree.with_node(el, |n| {
            assert_eq!(n.get_attribute_ns("http://www.w3.org/1999/xlink", "href"), Some("#a"));
            assert_eq!(n.get_attribute("xlink:href"), Some("#a"), "qualified-name lookup must find ns-set attrs");
        });

        // Update in place via NS API.
        tree.with_node_mut(el, |n| {
            n.set_attribute_ns("http://www.w3.org/1999/xlink", "xlink:href", "#b".into())
        });
        tree.with_node(el, |n| {
            assert_eq!(n.attrs().unwrap().len(), 1);
            assert_eq!(n.get_attribute("xlink:href"), Some("#b"));
        });

        tree.with_node_mut(el, |n| n.remove_attribute_ns("http://www.w3.org/1999/xlink", "href"));
        tree.with_node(el, |n| assert!(n.attrs().unwrap().is_empty()));
    }

    #[test]
    fn test_import_remaps_template_contents() {
        // A cloned <template> carries the SOURCE tree's contents NodeId, which
        // in the destination tree indexes an unrelated slot. The import must
        // allocate a fresh contents document and remap the reference.
        let source = crate::diting_dom::tree_sink::parse_html(
            r#"<div><template><span>tmpl</span></template></div>"#,
        );
        let dest = DomTree::new();
        let host = dest.new_node(NodeData::Element {
            name: QualName::new(None, ns!(html), local_name!("div")),
            attrs: vec![],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            live_value: None,
            live_checked: None,
        });
        dest.append_child(dest.document(), host);

        let src_div = source.query_selector("div").unwrap().unwrap();
        dest.import_children_from(host, &source, src_div);

        let imported_tmpl = dest.query_selector("template").unwrap().unwrap();
        let contents = dest
            .with_node(imported_tmpl, |n| match &n.data {
                NodeData::Element { template_contents, .. } => *template_contents,
                _ => None,
            })
            .flatten()
            .expect("imported template must carry a contents document");
        // The remapped contents node must live in DEST and hold the span.
        let text = dest.text_content(contents);
        assert_eq!(text, "tmpl");
        assert!(
            dest.with_node(contents, |n| n.is_document()).unwrap_or(false),
            "contents must be a Document node in the destination tree"
        );
    }

    #[test]
    fn test_deep_nesting_does_not_overflow() {
        // 20k nested divs: text_content and outer_html must complete without a
        // stack overflow (the recursive forms aborted the whole process).
        let depth = 20_000;
        let mut html = String::with_capacity(depth * 11 + 5);
        for _ in 0..depth {
            html.push_str("<div>");
        }
        html.push('x');
        let tree = crate::diting_dom::tree_sink::parse_html(&html);

        let text = tree.text_content(tree.document());
        assert_eq!(text, "x");
        let serialized = tree.outer_html(tree.document());
        assert!(serialized.len() >= depth * 5, "serialized len {}", serialized.len());
    }

    #[test]
    fn native_shadow_root_keeps_light_and_shadow_tree_scopes_separate() {
        let tree = DomTree::new();
        let document = tree.document();
        let host = element(&tree, "x-card");
        let light = element(&tree, "span");
        tree.append_child(document, host);
        tree.append_child(host, light);

        let root = tree
            .attach_shadow_root(host, ShadowRootMode::Closed)
            .expect("element can host one shadow root");
        let shadow = element(&tree, "button");
        tree.append_child(root, shadow);

        assert_eq!(
            tree.shadow_root_info(root),
            Some(ShadowRoot {
                id: root,
                host,
                mode: ShadowRootMode::Closed,
            })
        );
        assert!(tree.is_shadow_root(root));
        assert_eq!(tree.shadow_root(host), Some(root));
        assert_eq!(tree.get_node(root).unwrap().parent, None);
        assert_eq!(tree.children(host), vec![light]);
        // The root owns an ordinary child list — that list IS the shadow tree.
        assert_eq!(tree.children(root), vec![shadow]);

        let document_nodes = tree.descendants(document);
        assert!(!document_nodes.contains(&root));
        assert!(!document_nodes.contains(&shadow));
        assert_eq!(tree.tree_scope_root(light), Some(document));
        assert_eq!(tree.tree_scope_root(root), Some(root));
        assert_eq!(tree.tree_scope_root(shadow), Some(root));
        assert_eq!(tree.containing_shadow_root(light), None);
        assert_eq!(tree.containing_shadow_root(shadow), Some(root));
        assert_eq!(tree.shadow_including_root(shadow), Some(document));
        assert_eq!(
            tree.attach_shadow_root(host, ShadowRootMode::Open),
            Err(AttachShadowError::HostAlreadyHasShadowRoot)
        );
    }

    #[test]
    fn slot_assignment_uses_exact_names_first_slot_and_fallback_children() {
        let tree = DomTree::new();
        let host = element(&tree, "x-card");
        tree.append_child(tree.document(), host);
        let named = element(&tree, "span");
        tree.with_node_mut(named, |node| node.set_attribute("slot", "title".into()));
        let default_text = tree.new_node(NodeData::Text {
            contents: "default".into(),
        });
        tree.append_child(host, named);
        tree.append_child(host, default_text);

        let root = tree
            .attach_shadow_root(host, ShadowRootMode::Open)
            .unwrap();
        let first_named = element(&tree, "slot");
        tree.with_node_mut(first_named, |node| node.set_attribute("name", "title".into()));
        let duplicate_named = element(&tree, "slot");
        tree.with_node_mut(duplicate_named, |node| node.set_attribute("name", "title".into()));
        let fallback = element(&tree, "b");
        tree.append_child(duplicate_named, fallback);
        let default_slot = element(&tree, "slot");
        tree.append_child(root, first_named);
        tree.append_child(root, duplicate_named);
        tree.append_child(root, default_slot);

        assert_eq!(tree.assigned_slot(named), Some(first_named));
        assert_eq!(tree.assigned_slot(default_text), Some(default_slot));
        assert_eq!(tree.assigned_nodes(first_named), Some(vec![named]));
        // First same-name slot wins; the duplicate renders its fallback children.
        assert_eq!(tree.assigned_nodes(duplicate_named), Some(Vec::new()));
        assert_eq!(tree.children(duplicate_named), vec![fallback]);
        assert_eq!(tree.assigned_nodes(default_slot), Some(vec![default_text]));
    }

    #[test]
    fn document_id_lookup_never_exposes_a_shadow_descendant() {
        let tree = DomTree::new();
        let document = tree.document();
        let host = element(&tree, "x-card");
        tree.append_child(document, host);
        let root = tree
            .attach_shadow_root(host, ShadowRootMode::Open)
            .unwrap();

        // The shadow element is created first, so it owns the best-effort
        // global id-index entry. Public document lookup still has to recover
        // the light-tree match rather than leak across the tree scope.
        let shadow_match = element_with_id(&tree, "span", "shared");
        tree.append_child(root, shadow_match);
        let light_match = element_with_id(&tree, "span", "shared");
        tree.append_child(host, light_match);

        assert_eq!(tree.get_element_by_id("shared"), Some(light_match));
    }

    #[test]
    fn shadow_host_edges_participate_in_cycle_rejection() {
        let tree = DomTree::new();
        let document = tree.document();
        let host = element(&tree, "x-card");
        tree.append_child(document, host);
        let root = tree
            .attach_shadow_root(host, ShadowRootMode::Open)
            .unwrap();
        let shadow_child = element(&tree, "span");
        tree.append_child(root, shadow_child);

        // Root nodes cannot become ordinary children.
        tree.append_child(host, root);
        assert_eq!(tree.get_node(root).unwrap().parent, None);
        assert!(tree.children(host).is_empty());

        // A host is a host-including ancestor of every node in its shadow
        // tree, even when it has no light children.
        tree.append_child(root, host);
        tree.insert_before(shadow_child, host);
        assert_eq!(tree.get_node(host).unwrap().parent, Some(document));
        assert_eq!(tree.children(root), vec![shadow_child]);
    }

    #[test]
    fn freeing_a_host_reclaims_shadow_nodes_and_registry_entries() {
        let tree = DomTree::new();
        let host = element(&tree, "x-card");
        tree.append_child(tree.document(), host);
        let root = tree
            .attach_shadow_root(host, ShadowRootMode::Open)
            .unwrap();
        let shadow_host = element(&tree, "nested-card");
        tree.append_child(root, shadow_host);
        let nested_root = tree
            .attach_shadow_root(shadow_host, ShadowRootMode::Closed)
            .unwrap();
        let nested_child = element(&tree, "span");
        tree.append_child(nested_root, nested_child);

        assert_eq!(tree.len(), 6);
        tree.remove(host);
        assert_eq!(tree.len(), 1);
        for removed in [host, root, shadow_host, nested_root, nested_child] {
            assert!(tree.get_node(removed).is_none());
            assert!(!tree.is_shadow_root(removed));
        }

        // Reusing freed slots must not resurrect either registry direction.
        let replacement = element(&tree, "div");
        assert_eq!(tree.shadow_root(replacement), None);
        assert_eq!(tree.shadow_root_info(replacement), None);
    }

    #[test]
    fn cloning_a_host_omits_its_shadow_tree_and_a_root_is_not_clonable() {
        let tree = DomTree::new();
        let host = element(&tree, "x-card");
        let light = element(&tree, "span");
        tree.append_child(host, light);
        let root = tree
            .attach_shadow_root(host, ShadowRootMode::Open)
            .unwrap();
        tree.append_child(root, element(&tree, "button"));

        assert_eq!(tree.clone_node(root, true), None);
        let clone = tree.clone_node(host, true).expect("host itself is clonable");
        assert_eq!(tree.shadow_root(clone), None);
        assert_eq!(tree.children(clone).len(), 1);
    }

mod selector_tests {

    use crate::diting_dom::tree_sink::parse_html;

    #[test]
    fn test_query_selector_tag() {
        let tree = parse_html("<html><body><h1>Title</h1><p>Text</p></body></html>");
        let result = tree.query_selector("h1").unwrap();
        assert!(result.is_some());
        let node = tree.get_node(result.unwrap()).unwrap();
        assert_eq!(node.as_element().unwrap().local.as_ref(), "h1");
    }

    #[test]
    fn test_query_selector_class() {
        let tree =
            parse_html(r#"<div class="foo bar">Content</div><div class="baz">Other</div>"#);
        let result = tree.query_selector(".foo").unwrap();
        assert!(result.is_some());
        let node = tree.get_node(result.unwrap()).unwrap();
        assert_eq!(node.get_attribute("class"), Some("foo bar"));
    }

    #[test]
    fn test_query_selector_id() {
        let tree = parse_html(r#"<div id="main">Content</div>"#);
        let result = tree.query_selector("#main").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_query_selector_all() {
        let tree = parse_html("<ul><li>1</li><li>2</li><li>3</li></ul>");
        let results = tree.query_selector_all("li").unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_query_selector_descendant() {
        let tree =
            parse_html(r#"<div id="outer"><div id="inner"><span>Target</span></div></div>"#);
        let result = tree.query_selector("#outer span").unwrap();
        assert!(result.is_some());
        let node = tree.get_node(result.unwrap()).unwrap();
        assert_eq!(node.as_element().unwrap().local.as_ref(), "span");
    }

    #[test]
    fn test_query_selector_attribute() {
        let tree = parse_html(
            r#"<input type="text" name="user"><input type="password" name="pass">"#,
        );
        let result = tree.query_selector(r#"input[type="password"]"#).unwrap();
        assert!(result.is_some());
        let node = tree.get_node(result.unwrap()).unwrap();
        assert_eq!(node.get_attribute("name"), Some("pass"));
    }

    #[test]
    fn test_query_selector_no_match() {
        let tree = parse_html("<div>Hello</div>");
        let result = tree.query_selector("span").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_query_selector_complex() {
        let tree = parse_html(
            r#"<div class="container">
                <ul class="list">
                    <li class="item active">First</li>
                    <li class="item">Second</li>
                    <li class="item active">Third</li>
                </ul>
            </div>"#,
        );
        let results = tree.query_selector_all(".list .item.active").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_query_selector_all_from_scopes_to_subtree() {
        let tree = parse_html(
            r#"<div id="a"><span class="x">in a</span></div><div id="b"><span class="x">in b</span></div>"#,
        );
        let a = tree.get_element_by_id("a").expect("div#a");
        let b = tree.get_element_by_id("b").expect("div#b");

        // Document-rooted: sees both spans.
        assert_eq!(tree.query_selector_all(".x").unwrap().len(), 2);
        // Scoped to #a: only its descendant span.
        let in_a = tree.query_selector_all_from(a, ".x").unwrap();
        assert_eq!(in_a.len(), 1);
        let in_b = tree.query_selector_all_from(b, ".x").unwrap();
        assert_eq!(in_b.len(), 1);
        assert_ne!(in_a[0], in_b[0]);
    }

    #[test]
    fn test_query_selector_from_returns_first_in_subtree_only() {
        let tree = parse_html(
            r#"<section id="s"><p>first</p><p>second</p></section><p>outside</p>"#,
        );
        let s = tree.get_element_by_id("s").expect("section#s");

        // Scoped to #s: skip the outside paragraph; return the first inside.
        let first_in_s = tree.query_selector_from(s, "p").unwrap().expect("a p inside");
        assert_eq!(tree.text_content(first_in_s), "first");
    }

    #[test]
    fn test_root_pseudo_class_matches_document_element() {
        let tree = parse_html(r#"<html><body><div id="not-root"></div></body></html>"#);
        let hits = tree.query_selector_all(":root").unwrap();
        assert_eq!(hits.len(), 1, ":root matches exactly the document element");
        let html = hits[0];
        let node = tree.get_node(html).unwrap();
        assert_eq!(node.as_element().map(|q| q.local.as_ref()), Some("html"));
        assert!(tree.query_selector_all("div:root").unwrap().is_empty());
    }

    #[test]
    fn test_query_selector_from_excludes_self() {
        // The root element itself must not match its own scoped query, per
        // the spec: querySelector matches descendants only.
        let tree = parse_html(r#"<div id="root" class="x"><span>child</span></div>"#);
        let root = tree.get_element_by_id("root").expect("div#root");

        // Only descendants are candidates: `.x` on root finds nothing.
        assert!(tree.query_selector_from(root, ".x").unwrap().is_none());
        // `span` finds the child.
        assert!(tree.query_selector_from(root, "span").unwrap().is_some());
    }

    #[test]
    fn test_scope_child_combinator_scoped_to_query_root() {
        // The archify viewer pattern: container.querySelector(':scope > svg')
        // must find the container's direct svg child — not svgs in a sibling
        // container, not svgs deeper inside.
        let tree = parse_html(
            r#"<html><body>
                <div id="a"><svg id="direct"></svg><div><svg id="deep"></svg></div></div>
                <div id="b"><svg id="other"></svg></div>
            </body></html>"#,
        );
        let a = tree.get_element_by_id("a").unwrap();
        let hit = tree
            .query_selector_from(a, ":scope > svg")
            .unwrap()
            .expect("direct svg child of #a");
        assert_eq!(
            tree.get_element_by_id("direct"),
            Some(hit),
            "must be the direct child, not the deep or sibling svg"
        );

        let direct_only = tree.query_selector_all_from(a, ":scope > svg").unwrap();
        assert_eq!(direct_only.len(), 1);
    }

    #[test]
    fn test_scope_descendant_combinator_and_bare_form() {
        let tree = parse_html(
            r#"<html><body>
                <div id="a"><span class="x"></span><section><span class="x"></span></section></div>
                <span class="x"></span>
            </body></html>"#,
        );
        let a = tree.get_element_by_id("a").unwrap();
        // `:scope .x` = every .x in the subtree (both depths)...
        assert_eq!(tree.query_selector_all_from(a, ":scope .x").unwrap().len(), 2);
        // ...while a document-rooted query sees all three.
        assert_eq!(tree.query_selector_all(":scope .x").unwrap().len(), 3);

        // Bare `:scope` from an element root matches nothing: the scope
        // element itself is not among its own descendants.
        assert!(tree.query_selector_from(a, ":scope").unwrap().is_none());
        assert!(tree.query_selector_all_from(a, ":scope").unwrap().is_empty());
    }

    #[test]
    fn test_scope_from_document_root_means_html() {
        // Document-rooted queries leave the scope unset, where :scope falls
        // back to the root element — the browser behavior for
        // document.querySelector(':scope ...').
        let tree = parse_html(r#"<html><body><svg id="top"></svg><div><svg id="nested"></svg></div></body></html>"#);
        assert_eq!(
            tree.query_selector(":scope").unwrap(),
            tree.query_selector("html").unwrap(),
            "bare :scope from the document is the document element"
        );
        let hit = tree
            .query_selector(":scope > body")
            .unwrap()
            .expect("body is the document element's direct child");
        assert_eq!(
            tree.query_selector("body").unwrap(),
            Some(hit),
            ":scope from the document means html, so :scope > body resolves"
        );
    }

    #[test]
    fn test_matches_selector_binds_scope_to_element() {
        // Element.matches semantics: :scope is the tested element.
        let tree = parse_html(r#"<html><body><div id="hit" class="panel"><span></span></div></body></html>"#);
        let hit = tree.get_element_by_id("hit").unwrap();
        assert!(tree.matches_selector(hit, ":scope").unwrap());
        assert!(tree.matches_selector(hit, ":scope.panel").unwrap());
        assert!(!tree.matches_selector(hit, ":scope.missing").unwrap());
        // With :scope bound to the tested element, a child combinator off it
        // can only be false: nothing is its own parent. `span.matches
        // (':scope > span')` asks whether span's parent is span.
        let span = tree.query_selector("span").unwrap().expect("the span");
        assert!(!tree.matches_selector(span, ":scope > span").unwrap());
    }

    #[test]
    fn test_quirks_mode_class_id_case_insensitive() {
        // No doctype => full quirks mode: class and id selectors match
        // ASCII-case-insensitively (legacy pages depend on this).
        let quirks = parse_html(r#"<div class="Foo" id="Main">x</div>"#);
        assert!(quirks.is_quirks(), "no-doctype document must parse as quirks");
        assert!(quirks.query_selector(".foo").unwrap().is_some());
        assert!(quirks.query_selector(".FOO").unwrap().is_some());
        assert!(quirks.query_selector("#main").unwrap().is_some());

        // With a doctype => no-quirks: class/id match case-sensitively.
        let strict = parse_html(
            r#"<!DOCTYPE html><html><body><div class="Foo" id="Main">x</div></body></html>"#,
        );
        assert!(!strict.is_quirks());
        assert!(strict.query_selector(".Foo").unwrap().is_some());
        assert!(strict.query_selector(".foo").unwrap().is_none());
        assert!(strict.query_selector("#main").unwrap().is_none());
    }

    #[test]
    fn test_enabled_disabled_checked_match_dom_state() {
        let tree = parse_html(
            r#"<form>
                <input type="text" name="a">
                <input type="text" name="b" disabled>
                <input type="checkbox" name="c" checked>
                <button name="d">go</button>
                <select name="e"><option selected>x</option><option>y</option></select>
            </form>"#,
        );

        let enabled = tree.query_selector_all("input:enabled").unwrap();
        assert_eq!(enabled.len(), 2, "two non-disabled inputs: {enabled:?}");
        let disabled = tree.query_selector_all("input:disabled").unwrap();
        assert_eq!(disabled.len(), 1);
        let checked = tree.query_selector_all(":checked").unwrap();
        assert_eq!(checked.len(), 2, "checkbox[checked] + option[selected]: {checked:?}");

        // :enabled on a non-form-control never matches.
        let tree2 = parse_html(r#"<div>plain</div>"#);
        assert!(tree2.query_selector("div:enabled").unwrap().is_none());
    }

    #[test]
    fn test_has_parses() {
        // Isolate parse from match: :has must parse, not error.
        assert!(
            crate::diting_dom::selector::parse_selector("a:has(p.bt)").is_ok(),
            ":has failed to parse"
        );
    }

    #[test]
    fn test_query_selector_has() {
        let tree = parse_html(r#"<a><p class="bt">x</p></a>"#);
        let all = tree.query_selector_all("a:has(p.bt)").unwrap();
        assert_eq!(all.len(), 1, "a:has(p.bt) should match the <a>");
        let none = tree.query_selector_all("a:has(span.bt)").unwrap();
        assert_eq!(none.len(), 0, "a:has(span.bt) should match nothing");
    }

    #[test]
    fn test_query_selector_has_deep_and_bare_relative() {
        // :has with a descendant combinator several levels deep, and the bare
        // relative form (no leading combinator == descendant). Nested :has is
        // spec-invalid (:has(:has())) and the selectors crate rejects it.
        let tree = parse_html(
            r#"<section><article><div><ul><li class="hit">x</li></ul></div></article></section>"#,
        );
        assert_eq!(tree.query_selector_all("section:has(.hit)").unwrap().len(), 1);
        assert_eq!(tree.query_selector_all("article:has(li)").unwrap().len(), 1);
        assert_eq!(tree.query_selector_all("div:has(> ul > .hit)").unwrap().len(), 1);
        assert!(crate::diting_dom::selector::parse_selector("section:has(article:has(li))").is_err());
    }

    #[test]
    fn test_is_and_where_parse_and_match() {
        // Tailwind preflight / modern resets wrap rules in :where(...).
        let tree = parse_html(
            r#"<button class="btn">go</button><input class="field"><div class="btn-like">x</div>"#,
        );
        assert_eq!(
            tree.query_selector_all(":where(button, input)").unwrap().len(),
            2
        );
        assert_eq!(
            tree.query_selector_all(":is(button, input)").unwrap().len(),
            2
        );
        // Compound subject: .control:is(...) narrows by both parts.
        assert_eq!(
            tree.query_selector_all(r#"input:is([class="field"])"#.trim_end_matches('"')).is_err(),
            false
        );
        assert_eq!(tree.query_selector_all("button:where(.btn)").unwrap().len(), 1);
    }

    #[test]
    fn focus_state_pseudo_classes_parse_and_match_static_snapshot() {
        // Bootstrap's .visually-hidden-focusable pattern: :not(:focus):not(:focus-within)
        // must MATCH when nothing holds focus (both pseudos are false).
        let tree = parse_html(r#"<div class="visually-hidden-focusable">Skip</div>"#);
        let hidden = tree
            .query_selector_all(".visually-hidden-focusable:not(:focus):not(:focus-within)")
            .unwrap();
        assert_eq!(hidden.len(), 1);
        assert_eq!(tree.query_selector_all(":focus-visible").unwrap().len(), 0);
    }

    /// The live side of the focus pseudos (blitz#839): the tree's focused
    /// node drives :focus, :focus-within climbs ancestors, and
    /// :focus-visible narrows to text-entry controls (input type=text
    /// yes, type=checkbox no).
    #[test]
    fn focus_pseudo_classes_match_live_focused_node() {
        let tree = parse_html(
            r#"<form><input id="q" type="text"><input id="c" type="checkbox"><button id="b">Go</button></form>"#,
        );
        let q = tree.get_element_by_id("q").unwrap();
        tree.set_focused_node(Some(q));
        assert_eq!(tree.query_selector_all(":focus").unwrap().len(), 1);
        assert_eq!(
            tree.query_selector_all("#q:focus-visible").unwrap().len(),
            1
        );
        // :focus-within reaches the form even though the form itself isn't focused.
        assert_eq!(
            tree.query_selector_all("form:focus-within").unwrap().len(),
            1
        );

        // A checkbox holds focus → :focus matches, but :focus-visible doesn't
        // (mouse-ish control per the heuristic).
        let c = tree.get_element_by_id("c").unwrap();
        tree.set_focused_node(Some(c));
        assert_eq!(tree.query_selector_all("#c:focus").unwrap().len(), 1);
        assert_eq!(tree.query_selector_all(":focus-visible").unwrap().len(), 0);

        // Blur → every focus pseudo is false again.
        tree.set_focused_node(None);
        assert_eq!(tree.query_selector_all(":focus").unwrap().len(), 0);
        assert_eq!(
            tree.query_selector_all("form:focus-within").unwrap().len(),
            0
        );
    }

    #[test]
    fn link_pseudo_class_matches_anchor_with_href() {
        let tree = parse_html(
            r#"<a href="https://x/">real</a><a name="anchor-only">not</a><a>bare</a>"#,
        );
        let links = tree.query_selector_all(":link").unwrap();
        assert_eq!(links.len(), 1, ":link needs an href: {links:?}");
        assert_eq!(tree.query_selector_all("a:any-link").unwrap().len(), 1);
        // :visited is always false on a static snapshot.
        assert_eq!(tree.query_selector_all(":visited").unwrap().len(), 0);
    }

    #[test]
    fn matches_selector_works_on_detached_subjects() {
        // query_selector only walks the tree, so a detached subject was
        // untestable before matches_selector existed.
        let detached = parse_html("<em id='d'>d</em>");
        let d = detached.get_element_by_id("d").unwrap();
        assert!(detached.matches_selector(d, "em#d").unwrap());
        assert!(!detached.matches_selector(d, "div em").unwrap());
    }

    /// The rule-hash must be indistinguishable from the old per-rule
    /// querySelectorAll on every selector shape compute_styles feeds it:
    /// per-rule hit sets AND per-rule specificity, rule index by rule
    /// index. parse_html fixtures without a doctype parse in quirks mode,
    /// so the case-folded selectors in the list also pin the quirks
    /// bucket/probe spellings against the matcher's ground truth.
    #[test]
    fn rule_match_sets_agree_with_per_rule_qsa() {
        let tree = parse_html(
            r#"<div id="wrap" class="container">
                <h1 class="title main">T</h1>
                <p data-kind="lead">L</p>
                <p>plain</p>
                <ul><li class="item">1</li><li>2</li></ul>
                <svg><clipPath id="cp"><rect/></clipPath></svg>
                <span>s</span>
            </div>"#,
        );
        let selectors = [
            "div",
            ".container",
            "#wrap",
            "p",
            ".title.main",
            "div .item",
            "ul li",
            "h1, .item",
            "li, rect, #cp",
            "*",
            "ul *",
            "[data-kind]",
            "p[data-kind='lead']",
            "li:not(.item)",
            "li:first-child",
            "clipPath",
            "#nope",
            ".container, .missing",
            "p[lang]",
            // Quirks-mode case folds: whichever way the matcher answers,
            // the hash must answer identically.
            ".ITEM",
            "#WRAP",
            // Dangling combinator: fails to parse, so no bucket, no hits,
            // no specificity — same as the old qSA error path skipping it.
            "div >",
        ];
        let sets = tree.rule_match_sets(&selectors);
        for (ri, sel) in selectors.iter().enumerate() {
            let expected: Vec<usize> = tree
                .query_selector_all_from(tree.document(), sel)
                .map(|v| v.into_iter().map(|n| n.index()).collect())
                .unwrap_or_default();
            let got = sets.hits.get(&ri).cloned().unwrap_or_default();
            assert_eq!(got, expected, "hit set diverged for rule {ri} `{sel}`");
            // Ascending order is the cascade's binary-search contract.
            assert!(
                got.windows(2).all(|w| w[0] < w[1]),
                "hits not ascending for rule {ri} `{sel}`: {got:?}"
            );
            assert_eq!(
                sets.specificity[ri],
                tree.compile_rule_selector(sel).map(|c| c.specificity()),
                "specificity diverged for rule {ri} `{sel}`"
            );
        }
    }

    /// The incremental face must be indistinguishable from a fresh full
    /// match after every mutation shape the dirty registry stamps:
    /// re-parenting, attribute writes, detach-without-free, re-insertion,
    /// remove-with-free, and insert_before. The fixture parses quirks-mode
    /// (no doctype) so the case-folded bucket spellings ride along, and it
    /// is big enough that small mutations stay under the mass-mutation
    /// threshold — exercising the true incremental path, not the fallback.
    #[test]
    fn incremental_match_sets_equal_full_rebuild() {
        let mut lis = String::new();
        for i in 0..24 {
            let class = if i % 3 == 0 { format!("odd3 item{i}") } else { format!("item{i}") };
            lis.push_str(&format!("<li class='{class}'>{i}</li>"));
        }
        let html = format!(
            r#"<div id="wrap" class="container">
                <h1 class="title main">T</h1>
                <p data-kind="lead">L</p>
                <ul id="list">{lis}</ul>
                <section class="sink"><span class="ghost" data-k="g">g</span></section>
            </div>"#
        );
        let tree = parse_html(&html);
        let selectors = [
            "div",
            ".container",
            "#wrap",
            "p",
            ".title.main",
            "div .item",
            "ul li",
            "li.odd3 + li",
            "li ~ li",
            "li:first-child",
            "li:last-child",
            "li:nth-child(2n)",
            "[data-kind]",
            "p[data-kind='lead']",
            ".sink .ghost",
            "h1, .item",
            "*",
            "#nope",
            ".ITEM",
            "#WRAP",
            "div >",
            "span[data-k]",
            "ul:has(.odd3)",
            "div:not(.sink) > section",
            ".sinkitem",
        ];
        let key = 7u64;
        let check = |stage: &str| {
            let inc = tree.rule_match_sets_incremental(&selectors, key);
            let full = tree.rule_match_sets(&selectors);
            assert_eq!(inc, full, "incremental diverged at `{stage}`");
        };

        // First run: parse-time stamps cross the threshold, so this lands
        // on the full-rebuild path and populates the cache.
        check("initial");

        // 1. Re-parent: the ul moves into the section (roots + both child
        // lists + :has chains).
        let ul = tree.get_element_by_id("list").unwrap();
        let section = tree.query_selector_all(".sink").unwrap()[0];
        tree.append_child(section, ul);
        check("reparent ul");

        // 2. Attribute write through the ops-path shape: the h1 gains the
        // attr an unkeyed selector reads.
        let h1 = tree.query_selector_all("h1").unwrap()[0];
        let before = tree.rule_match_sets_incremental(&selectors, key);
        let unkeyed_ri = selectors.iter().position(|s| *s == "[data-kind]").unwrap();
        let before_hits = before.hits.get(&unkeyed_ri).cloned().unwrap_or_default().len();
        tree.with_node_mut(h1, |n| n.set_attribute("data-kind", "lead".into()));
        tree.note_restyle(h1);
        check("attr write");
        let after = tree.rule_match_sets_incremental(&selectors, key);
        let after_hits = after.hits.get(&unkeyed_ri).cloned().unwrap_or_default().len();
        assert_eq!(after_hits, before_hits + 1, "attr write must add the h1 hit");

        // 3. Class flip on one li.
        let li5 = tree.query_selector_all("li").unwrap()[5];
        tree.with_node_mut(li5, |n| n.set_attribute("class", "sinkitem".into()));
        tree.note_restyle(li5);
        check("class flip");
        let got = tree.rule_match_sets_incremental(&selectors, key);
        let sink_ri = selectors.iter().position(|s| *s == ".sinkitem").unwrap();
        assert_eq!(got.hits.get(&sink_ri).map(|v| v.len()), Some(1));

        // 4. Detach without free (remove_child), then re-insert elsewhere.
        let ghost = tree.query_selector_all(".ghost").unwrap()[0];
        tree.remove_child(ghost);
        check("detach ghost");
        tree.append_child(ul, ghost);
        check("reinsert ghost");

        // 5. Remove with free (slot recycling — stale indices must purge).
        let victim = tree.query_selector_all("li").unwrap()[1];
        tree.remove(victim);
        check("remove li");

        // 6. insert_before: the last li moves to the front.
        let lis_now = tree.query_selector_all("li").unwrap();
        tree.insert_before(lis_now[0], lis_now[lis_now.len() - 1]);
        check("insert_before li");

        // 7. Mass mutation: re-append every li — crosses the stamp-count
        // threshold and lands on the fallback rebuild.
        for li in tree.query_selector_all("li").unwrap() {
            tree.append_child(ul, li);
        }
        check("mass re-append");

        // 8. No mutations: the sync is a pure cache read.
        check("idle");

        // 9. A different css key drops the cache and rebuilds.
        let fresh = tree.rule_match_sets(&selectors);
        assert_eq!(tree.rule_match_sets_incremental(&selectors, key + 1), fresh);
    }

    /// Rules whose rightmost compound has no id/class/tag key (universal,
    /// attr-only, pseudo-only) fall into the always-tested bucket — the
    /// one place the hash could silently drop matches if the fallback
    /// regressed.
    #[test]
    fn rule_match_sets_unkeyed_rules_still_match() {
        let tree = parse_html(r#"<div><p data-x="1">a</p><p>b</p><span>c</span></div>"#);
        let selectors = ["[data-x]", "*:not(span)", "[hidden]", "*"];
        let sets = tree.rule_match_sets(&selectors);
        let expected: Vec<usize> = tree
            .query_selector_all_from(tree.document(), "[data-x]")
            .unwrap()
            .into_iter()
            .map(|n| n.index())
            .collect();
        let got = sets.hits.get(&0).cloned().unwrap_or_default();
        assert_eq!(got, expected, "attr-only rule lost its match");
        // `*` matches every element: html, head, body included.
        let star = sets.hits.get(&3).cloned().unwrap_or_default();
        assert!(star.len() >= 5, "universal rule matched only {star:?}");
        // Never-matching unkeyed rule contributes nothing.
        assert!(sets.hits.get(&2).is_none());
    }

    // ---- pseudo-element rule routing (::before/::after v1) ----
    use crate::diting_dom::selector::PseudoKind;

    #[test]
    fn pseudo_element_rules_route_out_of_normal_hits() {
        let tree = parse_html(r#"<div class="row">x</div><p class="row">y</p>"#);
        let sets = tree.rule_match_sets(&[".row:after"]);
        assert_eq!(sets.pseudo_kinds.get(&0), Some(&PseudoKind::After));
        assert_eq!(sets.pseudo_hits.get(&0).map(|v| v.len()), Some(2));
        assert!(
            !sets.hits.contains_key(&0),
            "pseudo rule must never reach the normal cascade"
        );
    }

    #[test]
    fn pseudo_suffix_spellings_and_case() {
        let tree = parse_html(r#"<ul><li id="a">one</li><li id="b">two</li></ul>"#);
        let sets = tree.rule_match_sets(&["li::before", "li:after", ".LI:BEFORE"]);
        assert_eq!(sets.pseudo_kinds.get(&0), Some(&PseudoKind::Before));
        assert_eq!(sets.pseudo_kinds.get(&1), Some(&PseudoKind::After));
        assert_eq!(sets.pseudo_hits.get(&0).map(|v| v.len()), Some(2));
        // The suffix matches case-insensitively but the base keeps the
        // author's case, so class matching stays case-sensitive: routing
        // is orthogonal to matching.
        assert_eq!(sets.pseudo_kinds.get(&2), Some(&PseudoKind::Before));
        assert!(sets.pseudo_hits.get(&2).is_none(), ".LI matches nothing");
    }

    #[test]
    fn comma_lists_and_non_trailing_suffixes_stay_dead() {
        let tree = parse_html(r##"<a id="x" href="#">l</a><b class="b">t</b>"##);
        let sets = tree.rule_match_sets(&["a::before, b", ".b a::before:hover", "a::before .b"]);
        assert!(
            sets.pseudo_kinds.is_empty(),
            "comma list / non-trailing suffix must not route"
        );
        assert!(sets.pseudo_hits.is_empty());
        assert!(!sets.hits.contains_key(&0), "comma'd pseudo list never matches real elements");
        assert!(!sets.hits.contains_key(&2), "descendant-of-pseudo never matches real elements");
    }
}
}
