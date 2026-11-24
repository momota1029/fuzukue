# Fixture setup shared by the screenshot runner and its server test.

shot_scenario_uses() {
    python3 -c 'import json,sys; sys.exit(not any(step["action"] == sys.argv[2] for step in json.load(open(sys.argv[1]))["steps"]))' "$1" "$2"
}

prepare_shot_git_fixture() {
    local run=$1 root=$2 scenario=$3 git_fixture=$4 workshop_fixture=$5
    local issue_threads_fixture=false
    if python3 -c 'import json,sys; sys.exit(not any(step.get("event") == "git_hub_activity" for step in json.load(open(sys.argv[1]))["steps"]))' "$scenario"; then
        workshop_fixture=true
        issue_threads_fixture=true
    fi
    if shot_scenario_uses "$scenario" wait_research_issue; then
        workshop_fixture=true
    fi
    local branch_review_fixture=false
    if python3 -c 'import json,sys; sys.exit(not any(step.get("id") == "tool-branch-review" for step in json.load(open(sys.argv[1]))["steps"]))' "$scenario"; then
        branch_review_fixture=true
    fi
    if [[ $git_fixture == false && $workshop_fixture == false && $branch_review_fixture == false ]]; then
        return
    fi

    local executable fixture=$run/home/project
    executable=$(command -v git) || { echo "Missing Git dependency: git" >&2; return 1; }
    ln -s "$executable" "$run/bin/git"
    fixture_git() {
        env -i PATH="$run/bin" HOME="$run/home" TMPDIR="$run/tmp" LC_ALL=C GIT_CONFIG_NOSYSTEM=1 \
            GIT_AUTHOR_DATE=2026-01-01T00:00:00Z GIT_COMMITTER_DATE=2026-01-01T00:00:00Z \
            git -C "$fixture" "$@"
    }

    if [[ $git_fixture == true ]]; then
        [[ ! -e $fixture/git-shot.txt && ! -L $fixture/git-shot.txt ]] || { echo "Git fixture path git-shot.txt is already in use" >&2; return 1; }
        printf '%s\n' 'before Git change' >"$fixture/git-shot.txt"
    fi
    fixture_git init -q -b main
    fixture_git config user.name 'Git fixture'
    fixture_git config user.email 'git-fixture@example.test'
    fixture_git add -A
    if [[ $git_fixture == true ]]; then
        fixture_git add -f -- git-shot.txt
    fi
    if [[ $branch_review_fixture == true ]]; then
        printf '%s\n' 'Review fixture' 'The result is unchecked.' >"$fixture/branch-review-shot.txt"
        fixture_git add -f -- branch-review-shot.txt
    fi
    fixture_git commit -qm 'フィクスチャの初期状態'
    if [[ $branch_review_fixture == true ]]; then
        local worker=$run/home/project-issue-1
        fixture_git worktree add -q -b work/issue-1 "$worker" main
        if [[ ${scenario##*/} == lean-statement-review.json ]]; then
            local lean_file=lean/LabSample/LabSample/Literate.lean
            python3 - "$worker/$lean_file" <<'PYTHON'
from pathlib import Path
import sys

path = Path(sys.argv[1])
source = path.read_text()
assert "theorem addZero (n : Nat) : n + 0 = n" in source
source = source.replace(
    "theorem addZero (n : Nat) : n + 0 = n",
    "theorem addZero (n : Nat) (_h : n > 0) : n + 0 = n",
).replace(":= addZero 1", ":= addZero 1 (by decide)")
path.write_text(source)
PYTHON
            (fixture=$worker fixture_git add -- "$lean_file")
            (fixture=$worker fixture_git commit -qm 'Change the addition lemma statement')
        else
            printf '%s\n' 'Review fixture' 'The result is verified.' 'Add an example for review.' >"$worker/branch-review-shot.txt"
            (fixture=$worker fixture_git add -- branch-review-shot.txt)
            (fixture=$worker fixture_git commit -qm 'Verify the fixture result')
        fi
    fi
    if [[ $git_fixture == true ]]; then
        printf '%s\n' 'after Git change' >"$fixture/git-shot.txt"
    fi
    if [[ $workshop_fixture == true ]]; then
        fixture_git remote add origin https://github.com/example-owner/lab.git
        cp "$root/tests/shots/gh" "$run/bin/"
    fi
    if [[ $issue_threads_fixture == true ]]; then
        cat >"$fixture/github-activity-fixture.json" <<'JSON'
{"shot":true,"issues":[{"number":13,"title":"Check the example and edge cases","url":"https://github.com/example-owner/lab/issues/13","state":"OPEN","body":"Verify the examples against the acceptance criteria.","labels":[{"name":"dot:review"},{"name":"tests"}]}]}
JSON
    fi
}
