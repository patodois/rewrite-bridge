"""Accepted text must be scanned completely, including above the small-blob cutoff."""
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

GATE = Path(__file__).with_name("check_public_tree.py")
MIB = 1024 * 1024

class PublicTreeTests(unittest.TestCase):
    def audit(self, data, config="", allow=""):
        with tempfile.TemporaryDirectory(prefix="public-tree-test-") as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / "probe.txt").write_bytes(data)
            if config:
                (root / ".public-tree.cfg").write_text(config)
            if allow:
                (root / ".public-tree-allow").write_text(allow)
            subprocess.run(["git", "-C", str(root), "add", "."], check=True)
            result = subprocess.run([sys.executable, str(GATE), "--repo", str(root), "--json"], capture_output=True, text=True)
            self.assertIn(result.returncode, (0, 1), result.stderr)
            return json.loads(result.stdout), result.returncode

    def test_markers_at_the_start_boundary_and_end_of_accepted_text(self):
        marker = ("Decompiled " + "with CFR").encode()
        for size, offset in [(100,0),(MIB+1,0),(MIB+100,MIB-5),(2*MIB,2*MIB-len(marker))]:
            data = b" " * offset + marker
            data += b" " * (size-len(data))
            with self.subTest(size=size, offset=offset):
                result, code = self.audit(data)
                self.assertEqual(code, 1, "accepted text escaped content scanning")
                self.assertIn("decompiler-marker", str(result))

    def test_text_limit_and_explicit_larger_limits_are_scanned(self):
        marker = ("Decompiled " + "with CFR").encode()
        clean = b" " * (2*MIB)
        self.assertEqual(self.audit(clean)[1], 0)
        self.assertEqual(self.audit(clean+b" ")[1], 1)
        larger = b" " * (2*MIB+10) + marker
        for config, allow in [("max_text_bytes 3145728\n", ""), ("", "probe.txt max=3145728 # synthetic text fixture\n")]:
            with self.subTest(config=config, allow=allow):
                result, code = self.audit(larger, config, allow)
                self.assertEqual(code, 1)
                self.assertIn("decompiler-marker", str(result))

TOKEN = "ghp_" + "a1B2" * 9           # a made-up GitHub token shape, built here so it is not a literal
PEM = "-----BEGIN RSA " "PRIVATE KEY-----\nabc\n"


class Repo(object):
    """A throwaway repository for the gate to audit, with the user's own Git configuration kept out."""

    def __init__(self, test):
        self.dir = tempfile.TemporaryDirectory(prefix="public-tree-repo-")
        test.addCleanup(self.dir.cleanup)
        self.root = Path(self.dir.name)
        self.env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0")
        self.git("init", "-q", "-b", "main")

    def git(self, *args, stdin=None):
        done = subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-C", str(self.root)]
                              + list(args), check=True, capture_output=True, env=self.env, input=stdin)
        return done.stdout.decode().strip()

    def write(self, name, data):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data if isinstance(data, bytes) else data.encode())

    def commit(self, message="c", force=False):
        self.git("add", "-f" if force else "-A", ".")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def gate(self, *args, stdin=None):
        done = subprocess.run([sys.executable, str(GATE), "--repo", str(self.root), "--json"] + list(args),
                              capture_output=True, text=True, env=self.env, input=stdin)
        hits = json.loads(done.stdout)["hits"] if done.stdout.strip() else []
        return done.returncode, sorted({h["rule"] for h in hits}), hits


class ContentFormsTests(unittest.TestCase):
    """Text the content rules must see whatever the encoding or the bytes around it."""

    def audit_file(self, data, name="a.txt"):
        repo = Repo(self)
        repo.write(name, data)
        repo.git("add", "-A")
        return repo.gate()[:2]

    def test_utf8_control(self):
        self.assertEqual(self.audit_file(TOKEN + "\n"), (1, ["secret-content"]))

    def test_utf16_text_is_searched(self):
        for codec in ("utf-16", "utf-16-le", "utf-16-be"):
            with self.subTest(codec=codec):
                self.assertEqual(self.audit_file(("Decompiled " + "with CFR\n" + TOKEN + "\n").encode(codec)),
                                 (1, ["decompiler-marker", "secret-content"]))

    def test_a_private_key_in_utf16_is_found(self):
        self.assertEqual(self.audit_file(PEM.encode("utf-16"), "notes.txt"), (1, ["secret-content"]))

    def test_a_nul_at_the_start_does_not_hide_text(self):
        self.assertEqual(self.audit_file(b"\0" + TOKEN.encode()), (1, ["secret-content"]))

    def test_the_raw_bytes_of_a_small_binary_are_searched(self):
        self.assertEqual(self.audit_file(b"\x7fELF\0\1\2\3" + TOKEN.encode() + b"\0\4", "tool.bin"),
                         (1, ["secret-content"]))

    def test_a_game_package_in_utf16_java_is_found(self):
        repo = Repo(self)
        repo.write(".public-tree.cfg", "allow_source src/**\n")
        repo.write("src/A.java", "package net.minecraft.world;\nclass A {}\n".encode("utf-16"))
        repo.git("add", "-A")
        self.assertEqual(repo.gate()[:2], (1, ["game-package-decl"]))

    def test_a_clean_binary_and_clean_utf16_pass(self):
        self.assertEqual(self.audit_file(bytes(range(256)) * 20, "x.bin")[0], 0)
        self.assertEqual(self.audit_file("hello world\n".encode("utf-16"))[0], 0)


class HistoryTests(unittest.TestCase):
    def test_the_root_commit_is_audited_whatever_the_users_log_configuration(self):
        repo = Repo(self)
        repo.write("game.jar", "x")
        repo.commit("root", force=True)
        repo.git("rm", "-qf", "game.jar")
        repo.commit("remove")
        repo.git("config", "log.showRoot", "false")
        code, rules, _ = repo.gate("--history")
        self.assertEqual((code, rules), (1, ["private-extension"]))

    def test_a_token_in_a_commit_message_or_a_tag_message_is_found(self):
        repo = Repo(self)
        repo.write("a.txt", "x\n")
        repo.commit("add a, token " + TOKEN)
        self.assertEqual(repo.gate("--history")[:2], (1, ["secret-content"]))
        clean = Repo(self)
        clean.write("a.txt", "x\n")
        clean.commit("add a")
        self.assertEqual(clean.gate("--history")[0], 0)
        clean.git("tag", "-a", "v1", "-m", TOKEN)
        code, rules, hits = clean.gate("--history")
        self.assertEqual((code, rules), (1, ["secret-content"]))
        self.assertIn("tag message", hits[0]["path"])
        # a tag message is not part of a range audit of commits
        self.assertEqual(clean.gate("--range", "HEAD")[0], 0)

    def test_history_follows_merges_other_branches_and_all_refs(self):
        # -m: a file that exists only in the result of a merge
        repo = Repo(self)
        repo.write("a.txt", "x\n")
        repo.commit("base")
        repo.git("checkout", "-q", "-b", "side")
        repo.write("b.txt", "y\n")
        repo.commit("side")
        repo.git("checkout", "-q", "main")
        repo.write("c.txt", "z\n")
        repo.commit("main2")
        repo.git("merge", "-q", "--no-commit", "side")
        repo.write("evil.jar", "x")
        repo.commit("merge", force=True)
        repo.git("rm", "-qf", "evil.jar")
        repo.commit("remove it again")
        self.assertEqual(repo.gate("--history")[:2], (1, ["private-extension"]))
        # --all: a branch that is not checked out
        other = Repo(self)
        other.write("a.txt", "x\n")
        other.commit("base")
        other.git("checkout", "-q", "-b", "other")
        other.write("game.jar", "x")
        other.commit("jar", force=True)
        other.git("checkout", "-q", "main")
        self.assertEqual(other.gate("--history")[:2], (1, ["private-extension"]))
        self.assertEqual(other.gate()[0], 0)


class PrePushTests(unittest.TestCase):
    def test_only_what_the_remote_lacks_is_audited(self):
        repo = Repo(self)
        repo.write("a.txt", "x\n")
        good = repo.commit("one")
        repo.write("game.jar", "x")
        bad = repo.commit("two", force=True)
        zero = "0" * 40
        # a new branch with a bad commit: found
        self.assertEqual(repo.gate("--pre-push", stdin="refs/heads/main %s refs/heads/main %s\n" % (bad, zero))[0], 1)
        # an update that adds the bad commit on top of what the remote has: found
        self.assertEqual(repo.gate("--pre-push", stdin="refs/heads/main %s refs/heads/main %s\n" % (bad, good))[0], 1)
        # a clean commit on top of a remote that already has the bad one: not found again
        repo.git("rm", "-qf", "game.jar")
        clean = repo.commit("three")
        self.assertEqual(repo.gate("--pre-push", stdin="refs/heads/main %s refs/heads/main %s\n" % (clean, bad))[0], 0)
        # deleting a ref pushes nothing
        self.assertEqual(repo.gate("--pre-push", stdin="(delete) %s refs/heads/x %s\n" % (zero, bad))[0], 0)


class RuleTests(unittest.TestCase):
    def test_the_game_package_rule_sees_through_a_bom_and_a_comment(self):
        for head in ("", "\ufeff", "/* header */ ", "\ufeff/* a */\n/* b */ "):
            with self.subTest(head=head):
                repo = Repo(self)
                repo.write(".public-tree.cfg", "allow_source src/**\n")
                repo.write("src/A.java", head + "package net.minecraft.world;\nclass A {}\n")
                repo.git("add", "-A")
                self.assertEqual(repo.gate()[:2], (1, ["game-package-decl"]))
        repo = Repo(self)
        repo.write(".public-tree.cfg", "allow_source src/**\n")
        repo.write("src/A.java", "// package net.minecraft.world;\npackage com.example;\nclass A {}\n")
        repo.git("add", "-A")
        self.assertEqual(repo.gate()[0], 0)

    def test_a_holder_named_in_prose_of_a_notice_is_not_a_holder_it_vouches_for(self):
        repo = Repo(self)
        repo.write("NOTICE", "This mod is not affiliated with Mojang AB.\n")
        repo.write("a.txt", "// Copyright (c) 2020 Mojang AB\nclass A {}\n")
        repo.git("add", "-A")
        self.assertEqual(repo.gate()[:2], (1, ["foreign-copyright"]))
        vouched = Repo(self)
        vouched.write("NOTICE", "Portions: Copyright (c) 2020 Mojang AB, used under its licence.\n")
        vouched.write("a.txt", "// Copyright (c) 2020 Mojang AB\nclass A {}\n")
        vouched.git("add", "-A")
        self.assertEqual(vouched.gate()[0], 0)

    def test_blob_pins_hold_and_a_content_exemption_needs_one(self):
        repo = Repo(self)
        marker = "Decompiled " + "with CFR\n"
        repo.write("shim/A.txt", marker)
        repo.git("add", "-A")
        blob = repo.git("rev-parse", ":shim/A.txt")
        repo.write(".public-tree-allow", "content:shim/** blob=%s # our own shim\n" % blob[:12])
        self.assertEqual(repo.gate()[0], 0)
        # the same path with other content is not exempt
        repo.write("shim/A.txt", marker + "changed\n")
        repo.git("add", "-A")
        self.assertEqual(repo.gate()[:2], (1, ["decompiler-marker"]))
        # an exemption of content without a pin is refused outright
        repo.write(".public-tree-allow", "content:shim/** # our own shim\n")
        code = subprocess.run([sys.executable, str(GATE), "--repo", str(repo.root)], capture_output=True, text=True,
                              env=repo.env)
        self.assertEqual(code.returncode, 2)
        self.assertIn("needs blob=", code.stderr)

    def test_the_secrets_and_the_size_rules_report(self):
        repo = Repo(self)
        repo.write(".env", "A=1\n")
        repo.write("a.txt", "key AKIA" + "ABCDEFGHIJKLMNOP\n")
        repo.git("add", "-A")
        self.assertEqual(repo.gate()[:2], (1, ["secret-content", "secret-file"]))


if __name__ == "__main__":
    unittest.main()
