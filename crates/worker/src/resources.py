def _install():
    import zipimport
    import _stdlib

    path = _stdlib.path()
    original = zipimport.zipimporter.get_resource_reader
    made = []

    # Imported on the first read, since zipfile and importlib.resources would slow every start
    def reader_class():
        import contextlib
        import os
        import pathlib
        import shutil
        import tempfile
        import zipfile
        from importlib.resources import _common
        from importlib.resources.readers import ZipReader

        # zipfile opens an archive with plain open(), which reads the executable instead
        archive = zipfile.ZipFile(_stdlib.open())

        class Stored(zipfile.Path):
            pass

        def copy(path, target):
            if path.is_dir():
                os.mkdir(target)
                for child in path.iterdir():
                    copy(child, os.path.join(target, child.name))
                return
            with path.open("rb") as source, open(target, "wb") as copied:
                shutil.copyfileobj(source, copied)

        # The copy keeps its name, which pip reads a wheel's version from
        @_common.as_file.register(Stored)
        @contextlib.contextmanager
        def as_file(path):
            with tempfile.TemporaryDirectory() as folder:
                target = os.path.join(folder, path.name)
                copy(path, target)
                yield pathlib.Path(target)

        class Reader(ZipReader):
            def files(self):
                return Stored(archive, self.prefix)

        return Reader

    def reader(self, fullname):
        if self.archive != path:
            return original(self, fullname)
        if not made:
            made.append(reader_class())
        return made[0](self, fullname)

    zipimport.zipimporter.get_resource_reader = reader


_install()
del _install
