"""Default-ref dispatch must validate merged identity before candidate execution."""

import copy
import runpy
from pathlib import Path

import pytest

SCRIPT = Path(__file__).parents[1] / ".github/scripts/ci_candidate.py"
SHA = "a" * 40
MAIN = "b" * 40


def documents():
    return (
        {"full_name": "zackees/soldr", "default_branch": "main"},
        {"name": "main", "commit": {"sha": MAIN}},
        {
            "base_commit": {"sha": MAIN},
            "merge_base_commit": {"sha": SHA},
            "status": "behind",
        },
        [
            {
                "number": 2,
                "merged_at": "2026-10-01T00:00:00Z",
                "merge_commit_sha": "c" * 40,
                "head": {"sha": SHA},
                "base": {"ref": "main", "repo": {"full_name": "zackees/soldr"}},
            }
        ],
    )


def check(docs, **options):
    return runpy.run_path(str(SCRIPT))["validate_candidate"](
        *docs,
        repository="zackees/soldr",
        candidate_sha=SHA,
        invocation_ref="refs/heads/main",
        **options,
    )


def test_merged_head_ancestor_is_valid_without_branch_head_substitution():
    proof = check(documents())
    assert proof["candidate_sha"] == SHA
    assert proof["authoritative_default_sha"] == MAIN
    assert proof["merged_pull_requests"] == [2]


def test_exact_merge_commit_is_valid():
    docs = documents()
    docs[3][0]["merge_commit_sha"] = SHA
    docs[3][0]["head"]["sha"] = "d" * 40
    assert check(docs)["success"] is True


@pytest.mark.parametrize(
    "case",
    [
        "repository",
        "branch",
        "default_sha",
        "candidate",
        "ahead",
        "diverged",
        "unmerged",
        "foreign_base",
        "wrong_pr_sha",
        "no_pr",
    ],
)
def test_untrusted_or_unmerged_identity_refuses(case):
    docs = copy.deepcopy(documents())
    if case == "repository":
        docs[0]["full_name"] = "attacker/soldr"
    elif case == "branch":
        docs[1]["name"] = "other"
    elif case == "default_sha":
        docs[2]["base_commit"]["sha"] = "d" * 40
    elif case == "candidate":
        docs[2]["merge_base_commit"]["sha"] = "d" * 40
    elif case in ("ahead", "diverged"):
        docs[2]["status"] = case
    elif case == "unmerged":
        docs[3][0]["merged_at"] = None
    elif case == "foreign_base":
        docs[3][0]["base"]["repo"]["full_name"] = "attacker/soldr"
    elif case == "wrong_pr_sha":
        docs[3][0]["head"]["sha"] = "d" * 40
    else:
        docs[3].clear()
    with pytest.raises(ValueError):
        check(docs)


def test_default_branch_contract_supports_master():
    docs = copy.deepcopy(documents())
    docs[0]["default_branch"] = "master"
    docs[1]["name"] = "master"
    docs[3][0]["base"]["ref"] = "master"
    validator = runpy.run_path(str(SCRIPT))["validate_candidate"]
    assert (
        validator(
            *docs,
            repository="zackees/soldr",
            candidate_sha=SHA,
            invocation_ref="refs/heads/master",
        )["success"]
        is True
    )
    with pytest.raises(ValueError):
        check(docs)


def test_repository_endpoint_has_no_trailing_slash(monkeypatch):
    helper = runpy.run_path(str(SCRIPT))
    requests = []

    class Response:
        def __enter__(self):
            import io

            return io.StringIO("{}")

        def __exit__(self, *_):
            return False

    def capture(request, timeout):
        requests.append(request.full_url)
        assert timeout == 30
        return Response()

    monkeypatch.setattr(helper["urllib"].request, "urlopen", capture)
    helper["github_get"]("zackees/soldr", "", "test-token")
    assert requests == ["https://api.github.com/repos/zackees/soldr"]
