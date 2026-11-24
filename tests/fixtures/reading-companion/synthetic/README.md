Synthetic (invented) inputs; no real source is quoted.

    B=target/debug/reading-companion; F=tests/fixtures/reading-companion/synthetic
    $B indicator $F/review/review.json
    $B build_review_page $F/review --out /tmp/r.html --lang en
    $B check_review_page /tmp/r.html --spec $F/review
    $B build_prose_page $F/prose/explanation.md --out /tmp/p.html
    # MCP: check_step_ledger {"deck":"tests/fixtures/reading-companion/synthetic/review"}
