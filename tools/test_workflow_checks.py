"""Regression jobs must really execute, and failures must affect CI."""
import unittest
from workflow_checks import has_test_command

COMMAND = "python3 -m unittest discover -s tools -p 'test_*.py'"
WORKFLOW = """name: CI
on: [push, pull_request]
jobs:
  python-tests:
    runs-on: ubuntu-latest
    steps:
      - run: COMMAND
""".replace("COMMAND", COMMAND)


class WorkflowExecutionTests(unittest.TestCase):
    def test_active_commands_are_found(self):
        for text in (WORKFLOW, WORKFLOW.replace("- run: " + COMMAND, "- run: |\n          # harmless comment\n          " + COMMAND), WORKFLOW.replace("python3", ".workflow-tests/bin/python")):
            with self.subTest(text=text):
                self.assertTrue(has_test_command(text, "tools", "test_*.py"))

    def test_disabled_commented_or_nonexecuted_commands_do_not_count(self):
        mutations = {
            "commented step": WORKFLOW.replace("      - run:", "      # - run:"),
            "commented shell command": WORKFLOW.replace("- run: " + COMMAND, "- run: |\n          # " + COMMAND),
            "disabled job": WORKFLOW.replace("    runs-on:", "    if: false\n    runs-on:"),
            "disabled expression job": WORKFLOW.replace("    runs-on:", "    if: ${{ false }}\n    runs-on:"),
            "disabled step": WORKFLOW.replace("- run: " + COMMAND, "- if: false\n        run: " + COMMAND),
            "conditional step": WORKFLOW.replace("- run: " + COMMAND, "- if: github.event_name == 'pull_request'\n        run: " + COMMAND),
            "allowed job failure": WORKFLOW.replace("    runs-on:", "    continue-on-error: true\n    runs-on:"),
            "allowed step failure": WORKFLOW.replace("- run: " + COMMAND, "- continue-on-error: true\n        run: " + COMMAND),
            "echo": WORKFLOW.replace("- run: " + COMMAND, "- run: echo " + COMMAND),
            "shell condition": WORKFLOW.replace("- run: " + COMMAND, "- run: |\n          if false; then\n            " + COMMAND + "\n          fi"),
            "wrong directory": WORKFLOW.replace("- run: " + COMMAND, "- working-directory: elsewhere\n        run: " + COMMAND),
            "only in a label": WORKFLOW.replace("- run: " + COMMAND, "- name: " + COMMAND + "\n        run: true"),
        }
        for name, text in mutations.items():
            with self.subTest(mutation=name):
                self.assertFalse(has_test_command(text, "tools", "test_*.py"))

    def test_judge_requires_its_own_enabled_command(self):
        self.assertFalse(has_test_command(WORKFLOW, "tests/interop", "test_results.py"))
        both = WORKFLOW + "      - run: python3 -m unittest discover -s tests/interop -p test_results.py\n"
        self.assertTrue(has_test_command(both, "tests/interop", "test_results.py"))
        disabled = both.replace("      - run: python3 -m unittest discover -s tests/interop", "      - if: false\n        run: python3 -m unittest discover -s tests/interop")
        self.assertFalse(has_test_command(disabled, "tests/interop", "test_results.py"))


if __name__ == "__main__":
    unittest.main()
