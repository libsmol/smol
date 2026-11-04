import os
import shutil
import subprocess
import venv


class _Builder(venv.EnvBuilder):
    # The interpreter is libsmol itself, and a link with its name would start the agent
    def ensure_directories(self, env_dir):
        context = super().ensure_directories(env_dir)
        context.python_exe = "python3"
        context.env_exe = os.path.join(context.bin_path, "python3")
        context.env_exec_cmd = context.env_exe
        self.bin = context.bin_path
        return context


def _repair(folder):
    builder = _Builder(symlinks=True, with_pip=True)
    builder.create(folder)
    # ensurepip leaves an installed pip alone, also one that lost its script
    if shutil.which("pip", path=builder.bin) is None:
        python = os.path.join(builder.bin, "python3")
        subprocess.run([python, "-m", "pip", "uninstall", "-y", "pip"])
        builder.create(folder)
