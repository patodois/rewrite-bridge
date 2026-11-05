"""Find unconditional Python regression commands in parsed GitHub Actions workflows."""
import shlex
import yaml


def _enabled(node):
    condition = node.get("if", True)
    return (condition is True or condition in ("true", "${{ true }}")) and node.get("continue-on-error", False) is False


def _root_directory(node):
    return isinstance(node, dict) and node.get("working-directory", ".") == "." and "shell" not in node


def has_test_command(text, directory, pattern):
    """Fail closed on conditions, dependencies or shell scripts that cannot prove execution.

    The dedicated regression job uses plain Python commands, with no matrix or dependent
    jobs. YAML comments are discarded by the parser, and shell comments by shlex. This is
    deliberately a checker for that explicit workflow shape, not a shell interpreter.
    """
    try:
        workflow = yaml.safe_load(text)
    except yaml.YAMLError:
        return False
    if not isinstance(workflow, dict):
        return False
    defaults = workflow.get("defaults", {})
    if not isinstance(defaults, dict) or not _root_directory(defaults.get("run", {})):
        return False
    jobs = workflow.get("jobs", {})
    if not isinstance(jobs, dict):
        return False
    expected = ["-m", "unittest", "discover", "-s", directory, "-p", pattern]
    for job in jobs.values():
        if not isinstance(job, dict) or not _enabled(job) or "needs" in job or "strategy" in job:
            continue
        defaults = job.get("defaults", {})
        if not isinstance(defaults, dict) or not _root_directory(defaults.get("run", {})):
            continue
        steps = job.get("steps", [])
        if not isinstance(steps, list):
            continue
        for step in steps:
            if not isinstance(step, dict) or not _enabled(step) or not _root_directory(step):
                continue
            script = step.get("run")
            if not isinstance(script, str):
                continue
            try:
                commands = [shlex.split(line, comments=True) for line in script.splitlines()]
            except ValueError:
                continue
            commands = [args for args in commands if args]
            # Reject shell control flow, echo, prefixes and unrelated commands rather than
            # mistaking a command quoted inside them for an executed regression test.
            if not commands or any(args[0] not in ("python3", "python", ".workflow-tests/bin/python") or args[1:4] != ["-m", "unittest", "discover"] for args in commands):
                continue
            if any(args[1:] == expected for args in commands):
                return True
    return False
