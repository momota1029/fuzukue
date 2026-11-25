use std::fs;

use serde_json::{json, Value};

use super::{mcp::Tool, Graph, WorkspaceGraph};
use crate::test_support::TestDir;

fn call(graph: &Graph, tool: Tool, args: Value) -> Value {
    tool.call(graph, args, &graph.cache.project).unwrap()
}

#[test]
fn reports_issues_and_resolves_cross_repo_edges_and_citations() {
    let project = TestDir::new();
    let external = TestDir::new();
    let state = TestDir::new();
    external.write("deck.toml", "kind = 'research'");
    external.write(
        "cards/original.md",
        "---\nid: original\nkind: claim\n---\nOriginal.",
    );
    project.write(
        "book/deck.toml",
        &format!(
            "kind = 'book'\nbibliography = 'refs.bib'\n[external]\nsource = {:?}\n",
            external.path().to_string_lossy()
        ),
    );
    project.write("book/structure.yaml", "chapters:\n- title: First\n  items:\n  - {subsection: Opening}\n  - thm\n- title: Second\n  label: second\n  items: [lem, axiom]\n");
    project.write("book/refs.bib", "@book{bk, title={Book}}");
    project.write(
        "references/ref-1/ref.toml",
        "bib_key = 'article'\ntitle = 'Article'",
    );
    project.write(
        "book/cards/thm/card.md",
        r"---
id: thm
kind: theorem
status: proved
summary:  One line.
from: source#original
requires: [lem, axiom, missing]
links: [{to: lem, rel: see}]
---
See [[source:original]].
",
    );
    project.write(
        "book/cards/thm/statement.tex",
        r"See \Cref{later,second} and \citep[pp. 10--11]{bk, article, absent}.",
    );
    project.write("book/cards/thm/proof.tex", r"Proof. % \ref{commented}");
    project.write("book/cards/lem.md", "---\nid: lem\nkind: lemma\nstatus: stated\nlabel: later\nrequires: [thm, axiom]\n---\nLemma.");
    project.write(
        "book/cards/axiom.md",
        "---\nid: axiom\nkind: axiom\n---\nAssumed.",
    );
    project.write(
        "book/cards/orphan.md",
        "---\nid: orphan\nfrom: https://example.test/repo#original\n---\nOutside.",
    );
    let graph = Graph::open(project.path(), state.path());
    let issues = call(&graph, Tool::Issues, json!({"deck":"book"}));
    let issues = issues["issues"].as_array().unwrap();
    for kind in [
        "dangling_reference",
        "requires_cycle",
        "status_inconsistency",
        "forward_chapter_reference",
        "unreachable_card",
    ] {
        assert!(
            issues.iter().any(|issue| issue["type"] == kind),
            "{kind}: {issues:?}"
        );
    }
    let dangling: Vec<_> = issues
        .iter()
        .filter_map(|issue| issue["target"].as_str())
        .collect();
    assert_eq!(dangling, ["absent", "missing"]);

    let edges = call(
        &graph,
        Tool::Neighbors,
        json!({"card":"thm","direction":"out"}),
    );
    assert_eq!(edges["nodes"][0]["summary"], "One line.");
    for target in [
        "reference:book#bk",
        "reference:#ref-1",
        "section:book#2",
        "book#lem",
    ] {
        assert!(
            edges["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["to"] == target),
            "{edges}"
        );
    }
    assert!(edges["edges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|edge| edge["rel"] == "from" && edge["to"].as_str().unwrap().ends_with("#original")));
    assert!(edges["edges"].as_array().unwrap().iter().any(|edge| {
        edge["rel"] == "mentions" && edge["to"].as_str().unwrap().ends_with("#original")
    }));
    let remote = call(
        &graph,
        Tool::Neighbors,
        json!({"card":"orphan","edge_types":["from"]}),
    );
    assert_eq!(remote["nodes"][1]["type"], "origin");

    let paths = call(&graph, Tool::Path, json!({"from":"thm","to":"lem"}));
    let paths = paths["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 3);
    assert!(paths.iter().all(|path| path["length"] == 1));
    assert_eq!(
        call(
            &graph,
            Tool::Path,
            json!({"from":"thm","to":"axiom","max_len":0})
        )["paths"],
        json!([])
    );

    let overview = call(&graph, Tool::Overview, json!({"deck":"book"}));
    assert_eq!(
        overview["decks"][0]["sections"][0]["items"][0]["items"][0]["id"],
        "thm"
    );
}

#[test]
fn refreshes_two_thousand_cards_and_reuses_cached_inputs() {
    let project = TestDir::new();
    let state = TestDir::new();
    project.write("deck.toml", "kind = 'research'");
    for i in 0..2000 {
        let requires = if i == 0 {
            String::new()
        } else {
            format!("requires: [card-{}]\n", i - 1)
        };
        project.write(&format!("cards/{i:04}.md"), &format!(
            "---\nid: card-{i}\nkind: lemma\ntitle: Generated card {i}\n{requires}---\nThis is a generated statement.\n"
        ));
    }
    let graph = WorkspaceGraph::default();
    graph.refresh(project.path(), state.path());
    assert!(state.path().join("card-graph.json").is_file());

    let overview = graph.with_graph(project.path(), state.path(), true, |graph| {
        assert_eq!(graph.inputs.card_reads, 2000);
        call(graph, Tool::Overview, json!({}))
    });
    assert_eq!(overview["decks"][0]["card_count"], 2000);
    assert!(overview["decks"][0]["omitted_cards"].as_u64().unwrap() > 0);
    assert!(overview.to_string().len() < 15_000);

    let cache = Graph::open(project.path(), state.path());
    assert_eq!(cache.cache.cards.len(), 2000);
    assert_eq!(cache.inputs.card_reads, 0);

    project.write(
        "cards/0000.md",
        "---\nid: card-0\ntitle: Updated\n---\nNew marker.",
    );
    graph.invalidate();
    let result = graph.with_graph(project.path(), state.path(), true, |graph| {
        assert_eq!(graph.cache.cards.len(), 2000);
        assert_eq!(graph.inputs.card_reads, 1);
        call(
            graph,
            Tool::Search,
            json!({"query":"New marker", "limit":1}),
        )
    });
    let restored = Graph::open(project.path(), state.path());
    assert_eq!(restored.inputs.card_reads, 0);
    assert_eq!(
        call(
            &restored,
            Tool::Search,
            json!({"query":"New marker", "limit":1})
        ),
        result
    );
    assert_eq!(result["matches"][0]["card"]["title"], "Updated");
}

#[test]
fn invalidation_handles_structure_and_card_removal() {
    let project = TestDir::new();
    let state = TestDir::new();
    project.write("deck.toml", "kind = 'book'");
    project.write("structure.yaml", "sections: [{title: Chapter, items: [x]}]");
    project.write("cards/x/card.md", "---\nid: x\n---\nExample.");
    let graph = WorkspaceGraph::default();
    let run = |tool, args| {
        graph.with_graph(project.path(), state.path(), true, |graph| {
            call(graph, tool, args)
        })
    };
    assert_eq!(run(Tool::Overview, json!({}))["decks"][0]["card_count"], 1);
    project.write("cards/x/statement.tex", "不変量を保存する。別の文。");
    graph.invalidate();
    assert_eq!(
        run(Tool::Search, json!({"query":"不変量"}))["matches"][0]["card"]["summary"],
        "不変量を保存する。"
    );
    fs::remove_dir_all(project.path().join("cards/x")).unwrap();
    graph.invalidate();
    assert_eq!(run(Tool::Issues, json!({}))["issues"][0]["target"], "x");
    project.write("structure.yaml", "sections: [{title: New chapter}]");
    graph.invalidate();
    assert_eq!(run(Tool::Issues, json!({}))["issues"], json!([]));
    assert_eq!(
        run(Tool::Overview, json!({}))["decks"][0]["sections"][0]["title"],
        "New chapter"
    );
}

#[test]
fn card_parts_exact_replace_and_scoped_checks() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let project = TestDir::new();
    let state = TestDir::new();
    project.write("deck.toml", "kind = 'book'\nimported_latex = true\n");
    let front = "---\nid: target\nkind: theorem\n---\n## Notes\n手元のノート\n";
    project.write("cards/sample/card.md", front);
    project.write("cards/sample/statement.tex", "\\label{thm:old}\nFirst line.\nSecond line.\n");
    project.write("cards/sample/proof.tex", "aaa\nUse the bound.\n");
    project.write("cards/linked.md", "---\nid: linked\n---\nSee \\ref{thm:old}.\n");
    project.write("cards/other.md", "---\nid: other\n---\nSee \\ref{unrelated}.\n");
    let graph = Graph::open(project.path(), state.path());

    assert_eq!(call(&graph, Tool::CardRead, json!({"card":"target","lines":"2-2"}))["text"], "First line.\n");
    assert_eq!(call(&graph, Tool::CardRead, json!({"card":"target","part":"notes"}))["text"], "手元のノート\n");
    let all = call(&graph, Tool::CardRead, json!({"card":"target","part":"all"}));
    assert!(all["text"].as_str().unwrap().contains("Use the bound."));
    assert!(all["text"].as_str().unwrap().contains("手元のノート"));
    assert_eq!(call(&graph, Tool::CardRead, json!({"card":"linked","part":"proof"}))["present"], false);

    let path = project.path().join("cards/sample/proof.tex");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&path).unwrap();
    for (old, expected) in [("aa", "複数箇所"), ("missing", "見つからない"), ("", "空")] {
        let args = json!({"card":"target", "part":"proof", "old":old, "new":""});
        let err = Tool::CardEdit.call(&graph, args, project.path()).unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
        assert_eq!(fs::metadata(&path).unwrap().ino(), before.ino());
        assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nUse the bound.\n");
    }
    let reply = call(&graph, Tool::CardEdit, json!({
        "card":"target", "part":"proof", "old":"Use the bound.", "new":"Apply the bound."
    }));
    assert_eq!(reply["diff"], "@@ 2 @@\n-Use the bound.\n+Apply the bound.");
    assert_eq!(reply["checks"]["cards_checked"], 2);
    assert_eq!(reply["checks"]["reference_count"], 0);
    let after = fs::metadata(&path).unwrap();
    assert_ne!(before.ino(), after.ino());
    assert_eq!(after.permissions().mode() & 0o777, 0o640);
    assert_eq!(fs::read_to_string(path).unwrap(), "aaa\nApply the bound.\n");
    assert_eq!(fs::read_to_string(project.path().join("cards/sample/card.md")).unwrap(), front);

    let reply = call(&graph, Tool::CardEdit, json!({
        "card":"target", "part":"statement", "old":"thm:old", "new":"thm:new"
    }));
    assert_eq!(reply["checks"]["references"], json!([{"card":".#linked","rel":"ref","target":"thm:old"}]));
    call(&graph, Tool::CardEdit, json!({"card":"target","part":"notes","old":"手元","new":"作業中"}));
    assert_eq!(fs::read_to_string(project.path().join("cards/sample/card.md")).unwrap(),
        front.replace("手元", "作業中"));
}

#[test]
fn large_book_tool_bytes() {
    const CHAPTERS: usize = 30;
    const CARDS: usize = 100;
    const OLD: &str = "old term";
    const NEW: &str = "new term";
    let project = TestDir::new();
    let state = TestDir::new();
    let mut main = String::from("\\documentclass{book}\n\\usepackage{amsthm}\n\\newtheorem{theorem}{Theorem}[chapter]\n\\begin{document}\n");
    let mut raw_bytes = 0;
    let prose = "The next argument identifies a representative object, develops the assumptions in order, and uses them to justify the claim. ".repeat(16);
    for chapter in 1..=CHAPTERS {
        let path = format!("raw/ch{chapter}.tex");
        main.push_str(&format!("\\input{{ch{chapter}}}\n"));
        let mut text = format!("\\chapter{{Chapter {chapter}}}\n");
        for card in 1..=CARDS {
            let term = if card % 10 == 0 { OLD } else { "the definition" };
            let reference = if chapter == 1 && card == 2 {
                "missing".to_owned()
            } else {
                format!("c{chapter}:{}", card.saturating_sub(1).max(1))
            };
            text.push_str(&format!(
                "\\begin{{theorem}}\\label{{c{chapter}:{card}}}\n\
                 Statement {card} uses {term}. See \\ref{{{reference}}}.\n\
                 {prose}\n\
                 \\end{{theorem}}\n\
                 \\begin{{proof}}\nThe bound follows by the first step.\n\\end{{proof}}\n"
            ));
        }
        raw_bytes += text.len();
        project.write(&path, &text);
    }
    main.push_str("\\end{document}\n");
    project.write("raw/main.tex", &main);
    let deck = project.path().join("book");
    let imported = fuzukue_cards::latex_book::import(&project.path().join("raw/main.tex"), &deck, false).unwrap();
    assert!(raw_bytes >= 6_000_000);
    assert_eq!(imported.cards, CHAPTERS * CARDS);
    let graph = Graph::open(project.path(), state.path());

    // Count the serialized arguments and one copy of the JSON result exposed
    // to the model. MCP duplicates the result for clients using TextContent.
    let measure = |tool, args: Value| {
        let size = args.to_string().len();
        let reply = call(&graph, tool, args);
        let bytes = size + reply.to_string().len();
        (reply, bytes)
    };
    let (overview, overview_bytes) = measure(Tool::Overview, json!({"deck":"book","depth":1}));
    assert!(overview.to_string().len() <= 6000);
    let sections = overview["decks"][0]["sections"].as_array().unwrap();
    assert_eq!(sections.len(), CHAPTERS);
    assert!(sections.iter().all(|s| s.get("items").is_none()));
    let key = sections[0]["key"].as_str().unwrap();
    let (chapter, chapter_bytes) = measure(Tool::Overview, json!({"chapter":key,"depth":2}));
    assert_eq!(chapter["cards"][0]["summary"], "Statement 1 uses the definition.");
    assert_eq!(chapter["card_count"], CARDS);
    let original = fs::read_to_string(project.path().join("raw/ch1.tex")).unwrap();
    let raw = original.len();

    println!("Book: {CHAPTERS} chapters, {} cards, {raw_bytes} bytes", imported.cards);
    println!("| Task | Raw read | Raw write | Tool read | Tool edit | Raw/tool |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
    for (task, card, part, old, new, range) in [
        ("Fix one proof", "c1-1", "proof", "first step", "base case", "1-4"),
        ("Fix dangling ref", "c1-2", "statement", r"\ref{missing}", r"\ref{c1:1}", "1-2"),
    ] {
        let card = format!("book#{card}");
        let (_, read) = measure(Tool::CardRead, json!({"card":card,"part":part,"lines":range}));
        let (_, edit) = measure(Tool::CardEdit, json!({"card":card,"part":part,"old":old,"new":new}));
        let reads = overview_bytes + chapter_bytes + read;
        let raw_write = raw - old.len() + new.len();
        let ratio = (raw + raw_write) as f64 / (reads + edit) as f64;
        println!("| {task} | {raw} | {raw_write} | {reads} | {edit} | {ratio:.2}× |");
        assert!(raw >= 10 * (reads + edit), "{task}: {raw} vs {}", reads + edit);
    }

    let mut reads = overview_bytes + chapter_bytes;
    let mut edits = 0;
    let mut changed = 0;
    let mut page = chapter;
    loop {
        for item in page["cards"].as_array().unwrap() {
            let card = format!("book#{}", item["id"].as_str().unwrap());
            let (reply, bytes) = measure(Tool::CardRead, json!({"card":card,"part":"statement"}));
            reads += bytes;
            if reply["text"].as_str().unwrap().contains(OLD) {
                let (_, bytes) = measure(Tool::CardEdit, json!({"card":card,"part":"statement","old":OLD,"new":NEW}));
                edits += bytes;
                changed += 1;
            }
        }
        let Some(offset) = page["next_offset"].as_u64() else { break; };
        let (reply, bytes) = measure(Tool::Overview, json!({"chapter":key,"depth":2,"offset":offset}));
        reads += bytes;
        page = reply;
    }
    assert_eq!(changed, 10);
    let raw_write = original.replace(OLD, NEW).len();
    let ratio = (raw + raw_write) as f64 / (reads + edits) as f64;
    println!("| Rename a chapter term | {raw} | {raw_write} | {reads} | {edits} | {ratio:.2}× |");
}

#[test]
fn symbols_update_changed_files_and_group_copies() {
    let project = TestDir::new();
    let state = TestDir::new();
    project.write("deck.toml", "kind = 'paper'");
    let card = "---\nid: a\ndefines: [a term]\n---\nDefinition.";
    project.write("cards/a.md", card);
    project.write("review/deck.toml", "kind = 'paper'");
    project.write("review/cards/a.md", card);
    let usage = "---\nid: b\n---\nA TERM appears here. $a term$ is math.";
    project.write("review/cards/b.md", usage);
    project.write("cards/b.md", usage);
    project.write("lean/a.lean", "namespace A\ndef existing := 1\nend A\n");
    project.write("paper.tex", r"\documentclass{article}\input{pieces/body}");
    project.write("pieces/body.tex", r"\label{tag} \ref{tag}");
    project.write("other.tex", r"\documentclass{article} \label{tag} \ref{tag}");
    let graph = WorkspaceGraph::default();
    let run = |tool, args| graph.with_graph(project.path(), state.path(), true, |graph| call(graph, tool, args));

    let result = run(Tool::Definition, json!({"name":"a term"}));
    let defs = result["definitions"].as_array().unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0]["alternates"][0]["deck"], "review");
    let result = run(Tool::References, json!({"id": defs[0]["id"]}));
    assert_eq!(result["references"].as_array().unwrap().len(), 1);
    assert_eq!(result["references"][0]["alternates"][0]["deck"], "review");
    assert_eq!(result["references"][0]["location"]["snippet"], "A TERM appears here. $a term$ is math.");

    let hits = run(Tool::Definition, json!({"name":"tag", "kind":"label"}));
    let hit = &hits["definitions"][1];
    assert_eq!(hit["location"]["file"], "pieces/body.tex");
    let refs = run(Tool::References, json!({"id":hit["id"]}));
    assert_eq!(refs["references"].as_array().unwrap().len(), 1);
    assert_eq!(refs["references"][0]["location"]["file"], "pieces/body.tex");

    project.write("lean/a.lean", "namespace A\ndef replacement := 123\nend A\n");
    graph.invalidate_paths(&[project.path().join("lean/a.lean")]);
    assert_eq!(run(Tool::Definition, json!({"name":"existing"}))["definitions"], json!([]));
    assert_eq!(run(Tool::Definition, json!({"name":"replacement"}))["definitions"][0]["qualified"], "A.replacement");
    fs::remove_file(project.path().join("review/cards/a.md")).unwrap();
    graph.invalidate_paths(&[project.path().join("review/cards/a.md")]);
    assert!(run(Tool::Definition, json!({"name":"a term"}))["definitions"][0]["alternates"].is_null());
}
