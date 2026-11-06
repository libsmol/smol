import os
import shutil
import subprocess
import venv


class _Builder(venv.EnvBuilder):
    # The interpreter is libsmol itself, and a link with its name would start the agent
    def ensure_directories(self, env_dir):
        context = super().ensure_directories(env_dir)
        # On Windows stdlib.py names it python.exe, in every venv
        if os.name != "nt":
            context.python_exe = _LINKED
            context.env_exe = os.path.join(context.bin_path, _LINKED)
            context.env_exec_cmd = context.env_exe
        self.bin = context.bin_path
        self.python = context.env_exe
        return context


def _repair(folder):
    builder = _Builder(symlinks=True, with_pip=True)
    builder.create(folder)
    # ensurepip leaves an installed pip alone, also one that lost its script
    if shutil.which("pip", path=builder.bin) is None:
        subprocess.run([builder.python, "-m", "pip", "uninstall", "-y", "pip"])
        builder.create(folder)
