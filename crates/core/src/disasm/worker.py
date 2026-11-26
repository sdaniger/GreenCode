#!/usr/bin/env python3
"""One disassembler database, driven by JSONL requests on stdin and replies on stdout.

Usage: worker.py <backend:ida|ghidra|binja> <binary> <database> <arch>

The first line is `{"ready": true, "info": {...}}` once analysis finished, or
`{"ready": false, "error": "..."}`. Each request is `{"id": n, "method": ..., "params": {...}}`;
each reply is `{"id": n, "ok": true, "result": ...}` or `{"id": n, "ok": false, "error": "..."}`.
Exec runs Python in this process, so the database objects and helpers below are in scope.
"""

import contextlib
import io
import json
import os
import sys
import traceback

MAX_LINES = 400


def _lines(rows, more_hint=""):
    rows = list(rows)
    out = rows[:MAX_LINES]
    if len(rows) > MAX_LINES:
        out.append(f"... ({len(rows) - MAX_LINES} more{more_hint})")
    return "\n".join(out)


def _hex(value):
    if isinstance(value, str):
        return value
    return f"0x{int(value):x}"


class Adapter:
    label = "?"

    def __init__(self, binary, database, arch):
        self.binary = binary
        self.database = database
        self.arch = arch

    # --- primitives every backend provides ---------------------------------
    def functions(self):
        raise NotImplementedError

    def pseudocode(self, ea):
        raise NotImplementedError

    def disassembly(self, ea):
        raise NotImplementedError

    def imports(self):
        raise NotImplementedError

    def exports(self):
        raise NotImplementedError

    def strings(self):
        raise NotImplementedError

    def xrefs_to(self, ea):
        raise NotImplementedError

    def xrefs_from(self, ea):
        raise NotImplementedError

    def segments(self):
        raise NotImplementedError

    def entries(self):
        raise NotImplementedError

    def read_bytes(self, ea, size):
        raise NotImplementedError

    def rename(self, ea, name):
        raise NotImplementedError

    def comment(self, ea, text):
        raise NotImplementedError

    def set_type(self, ea, decl):
        raise NotImplementedError

    def make_function(self, ea):
        raise NotImplementedError

    def save(self):
        raise NotImplementedError

    def close(self):
        pass

    # --- shared helpers ----------------------------------------------------
    def info(self):
        return {
            "backend": self.label,
            "path": self.binary,
            "format": getattr(self, "format", ""),
            "arch": getattr(self, "arch_name", self.arch or ""),
            "bits": getattr(self, "bits", 0),
        }

    def resolve(self, target):
        """A name or a 0x/integer address to an address; names resolve to a function or symbol."""
        if isinstance(target, int):
            return target
        text = str(target).strip()
        if text.lower().startswith("0x"):
            return int(text, 16)
        if text.isdigit():
            return int(text)
        for ea, name in self.functions():
            if name == text:
                return ea
        ea = getattr(self, "symbol_address", lambda _n: None)(text)
        if ea is None:
            raise KeyError(f"no function or symbol named {text!r}")
        return ea

    def function_at(self, ea):
        for f_ea, name in self.functions():
            if f_ea == ea:
                return f_ea, name
        raise KeyError(f"no function at {_hex(ea)}")

    def globals(self):
        """Extra names visible to exec besides the generic helpers."""
        return {}


class Ida(Adapter):
    label = "IDA"

    def __init__(self, binary, database, arch):
        super().__init__(binary, database, arch)
        import idapro

        # idalib ships its bindings under <IDA>/idalib/python; the caller puts that on PYTHONPATH.
        if not idapro.open_database(binary, run_auto_analysis=True):
            raise RuntimeError(f"ida_domain could not open {binary}")
        import ida_bytes
        import ida_entry
        import ida_funcs
        import ida_hexrays
        import ida_lines
        import ida_loader
        import ida_name
        import ida_nalt
        import ida_segment
        import ida_typeinf
        import ida_ua
        import ida_xref
        import idautils

        self.m = {
            "ida_bytes": ida_bytes, "ida_entry": ida_entry, "ida_funcs": ida_funcs,
            "ida_hexrays": ida_hexrays, "ida_lines": ida_lines, "ida_loader": ida_loader,
            "ida_name": ida_name, "ida_nalt": ida_nalt, "ida_segment": ida_segment,
            "ida_typeinf": ida_typeinf, "ida_ua": ida_ua, "ida_xref": ida_xref,
            "idautils": idautils, "idapro": idapro,
        }
        try:
            ida_hexrays.init_hexrays_plugin()
        except Exception:
            pass
        self.format = ida_loader.get_file_type_name() if hasattr(ida_loader, "get_file_type_name") else ""
        self.bits = 64 if ida_nalt.is_64bit() else 32
        self.arch_name = ida_nalt.get_processor_name() or ""

    def info(self):
        info = super().info()
        try:
            info["format"] = self.m["ida_loader"].get_file_type_name()
        except Exception:
            pass
        return info

    def functions(self):
        funcs = self.m["ida_funcs"]
        out = []
        for i in range(funcs.get_func_qty()):
            f = funcs.getn_func(i)
            if f is None:
                continue
            out.append((int(f.start_ea), funcs.get_func_name(f.start_ea) or f"sub_{f.start_ea:X}"))
        return out

    def _function(self, ea):
        f = self.m["ida_funcs"].get_func(ea)
        if f is None:
            raise KeyError(f"no function at {_hex(ea)}")
        return f

    def pseudocode(self, ea):
        f = self._function(ea)
        cfunc = self.m["ida_hexrays"].decompile(int(f.start_ea))
        if cfunc is None:
            raise RuntimeError("the decompiler returned nothing for this function")
        return str(cfunc)

    def disassembly(self, ea):
        f = self._function(ea)
        lines = []
        for item in self.m["idautils"].FuncItems(int(f.start_ea)):
            text = self.m["ida_lines"].generate_disasm_line(item, 0)
            if text:
                lines.append(f"{_hex(item):>12}  {self.m['ida_lines'].tag_remove(text).rstrip()}")
        return "\n".join(lines)

    def imports(self):
        nalt, out = self.m["ida_nalt"], []

        def add(ea, name, ordinal):
            out.append((int(ea), name or f"#{ordinal}"))

        for i in range(nalt.get_import_module_qty()):
            module = nalt.get_import_module_name(i) or "?"
            nalt.enum_import_names(i, lambda ea, name, ordinal: add(ea, f"{module}!{name}" if name else None, ordinal))
        return [r for r in out if r[0]]

    def exports(self):
        entry, out = self.m["ida_entry"], []
        for i in range(entry.get_entry_qty()):
            ordinal = entry.get_entry_ordinal(i)
            out.append((int(entry.get_entry(ordinal)), entry.get_entry_name(ordinal) or ""))
        return out

    def symbol_address(self, name):
        ea = self.m["ida_name"].get_name_ea_simple(name)
        return None if ea is None or ea == 0xFFFFFFFFFFFFFFFF else int(ea)

    def strings(self):
        out = []
        for s in self.m["idautils"].Strings():
            try:
                out.append((int(s.ea), str(s)))
            except Exception:
                continue
        return out

    def xrefs_to(self, ea):
        out = []
        for x in self.m["idautils"].XrefsTo(ea):
            out.append({"from": _hex(x.frm), "type": self.m["ida_xref"].xref_type_to_name(x.type)})
        return out

    def xrefs_from(self, ea):
        out = []
        for x in self.m["idautils"].XrefsFrom(ea):
            out.append({"to": _hex(x.to), "type": self.m["ida_xref"].xref_type_to_name(x.type)})
        return out

    def segments(self):
        seg, out = self.m["ida_segment"], []
        for i in range(seg.get_segm_qty()):
            s = seg.getnseg(i)
            if s is None:
                continue
            perms = "".join(c if s.perm & bit else "-" for bit, c in ((1, "r"), (2, "w"), (4, "x")))
            out.append({"name": seg.get_segm_name(s) or "", "start": int(s.start_ea), "end": int(s.end_ea), "perms": perms})
        return out

    def entries(self):
        e, out = self.m["ida_entry"], []
        start = self.m["ida_nalt"].get_imagebase()
        try:
            import ida_ida

            start = ida_ida.inf_get_start_ea()
        except Exception:
            pass
        if start:
            out.append((int(start), "entry"))
        for i in range(e.get_entry_qty()):
            ordinal = e.get_entry_ordinal(i)
            out.append((int(e.get_entry(ordinal)), e.get_entry_name(ordinal) or ""))
        return out

    def read_bytes(self, ea, size):
        data = self.m["ida_bytes"].get_bytes(ea, size)
        return bytes(data or b"")

    def rename(self, ea, name):
        old = self.m["ida_funcs"].get_func_name(ea) or self.m["ida_name"].get_name(ea) or ""
        if not self.m["ida_name"].set_name(ea, name, self.m["ida_name"].SN_NOWARN | self.m["ida_name"].SN_FORCE):
            raise RuntimeError(f"IDA refused the name {name!r}")
        return {"ea": _hex(ea), "old": old, "new": name}

    def comment(self, ea, text):
        self.m["ida_bytes"].set_cmt(ea, text, 0)
        try:
            f = self.m["ida_funcs"].get_func(ea)
            if f is not None and int(f.start_ea) == int(ea):
                self.m["ida_funcs"].set_func_cmt(f, text, 0)
        except Exception:
            pass
        return {"ea": _hex(ea)}

    def set_type(self, ea, decl):
        import idc

        if not idc.SetType(ea, decl):
            raise RuntimeError(f"IDA refused the type {decl!r}")
        return {"ea": _hex(ea), "type": decl}

    def make_function(self, ea):
        if not self.m["ida_funcs"].add_func(ea, -1):
            raise RuntimeError(f"IDA could not make a function at {_hex(ea)}")
        ea = int(self.m["ida_funcs"].get_func(ea).start_ea)
        return {"ea": _hex(ea), "name": self.m["ida_funcs"].get_func_name(ea) or ""}

    def save(self):
        loader = self.m["ida_loader"]
        if not loader.save_database(self.database, 0):
            raise RuntimeError("IDA could not save the database")
        return {"path": self.database}

    def close(self):
        try:
            self.m["ida_loader"].close_database(0)
        except Exception:
            pass

    def globals(self):
        return dict(self.m)


class Ghidra(Adapter):
    label = "Ghidra"

    def __init__(self, binary, database, arch):
        super().__init__(binary, database, arch)
        import pyghidra

        pyghidra.start()
        project = os.path.dirname(database) or "."
        name = os.path.splitext(os.path.basename(database))[0] or "greencode"
        self._cm = pyghidra.open_program(binary, project_location=project, project_name=name, analyze=True)
        self.flat = self._cm.__enter__()
        self.program = self.flat.getCurrentProgram()
        if self.program is None:
            raise RuntimeError(f"pyghidra did not open {binary}")
        from ghidra.app.decompiler import DecompInterface, DecompileOptions
        from ghidra.program.model.listing import CodeUnit
        from ghidra.util.task import ConsoleTaskMonitor

        self.CommentType = CodeUnit
        self.monitor = ConsoleTaskMonitor()
        self.decompiler = DecompInterface()
        self.decompiler.setOptions(DecompileOptions())
        self.decompiler.openProgram(self.program)
        lang = self.program.getLanguage()
        self.format = self.program.getExecutableFormat()
        self.arch_name = lang.getLanguageID().getIdAsString() if lang else ""
        self.bits = lang.getLanguageDescription().getSize() if lang else 0

    def functions(self):
        fm = self.program.getFunctionManager()
        out = []
        for f in fm.getFunctions(True):
            out.append((int(f.getEntryPoint().getOffset()), f.getName()))
        return out

    def _function(self, ea):
        f = self.program.getFunctionManager().getFunctionAt(self._addr(ea))
        if f is None:
            raise KeyError(f"no function at {_hex(ea)}")
        return f

    def _addr(self, ea):
        return self.program.getAddressFactory().getDefaultAddressSpace().getAddress(int(ea))

    def pseudocode(self, ea):
        res = self.decompiler.decompileFunction(self._function(ea), 60, self.monitor)
        if not res.decompileCompleted() or res.getDecompiledFunction() is None:
            raise RuntimeError(res.getErrorMessage() or "the decompiler returned nothing")
        return res.getDecompiledFunction().getC()

    def disassembly(self, ea):
        listing = self.program.getListing()
        out = []
        for insn in listing.getInstructions(self._function(ea).getBody(), True):
            out.append(f"{insn.getAddress()}: {insn}")
        return "\n".join(out)

    def imports(self):
        out = []
        for sym in self.program.getSymbolTable().getExternalSymbols():
            out.append((int(sym.getAddress().getOffset()), sym.getName()))
        return out

    def exports(self):
        table, out = self.program.getSymbolTable(), []
        for sym in table.getAllSymbols(True):
            if sym.isExternal() or not sym.isGlobal():
                continue
            out.append((int(sym.getAddress().getOffset()), sym.getName()))
        return out

    def symbol_address(self, name):
        syms = self.program.getSymbolTable().getGlobalSymbols(name)
        for sym in syms:
            return int(sym.getAddress().getOffset())
        return None

    def strings(self):
        out = []
        for data in self.program.getListing().getDefinedData(True):
            if data.hasStringValue():
                out.append((int(data.getAddress().getOffset()), str(data.getValue())))
        return out

    def xrefs_to(self, ea):
        out = []
        for ref in self.program.getReferenceManager().getReferencesTo(self._addr(ea)):
            out.append({"from": str(ref.getFromAddress()), "type": str(ref.getReferenceType())})
        return out

    def xrefs_from(self, ea):
        out = []
        for ref in self.program.getReferenceManager().getReferencesFrom(self._addr(ea)):
            out.append({"to": str(ref.getToAddress()), "type": str(ref.getReferenceType())})
        return out

    def segments(self):
        out = []
        for block in self.program.getMemory().getBlocks():
            out.append({
                "name": block.getName(),
                "start": int(block.getStart().getOffset()),
                "end": int(block.getEnd().getOffset()) + 1,
                "perms": ("r" if block.isRead() else "-") + ("w" if block.isWrite() else "-") + ("x" if block.isExecute() else "-"),
            })
        return out

    def entries(self):
        out = [(int(self.program.getEntryPoint().getOffset()), "entry")]
        for addr in self.program.getSymbolTable().getExternalEntryPointIterator():
            out.append((int(addr.getOffset()), "export"))
        return out

    def read_bytes(self, ea, size):
        buf = bytearray(size)
        self.program.getMemory().getBytes(self._addr(ea), buf)
        return bytes(buf)

    def rename(self, ea, name):
        from ghidra.program.model.symbol import SourceType

        sym = self.program.getSymbolTable().getPrimarySymbol(self._addr(ea))
        if sym is None:
            raise KeyError(f"no symbol at {_hex(ea)}")
        old = sym.getName()
        sym.setName(name, SourceType.USER_DEFINED)
        return {"ea": _hex(ea), "old": old, "new": name}

    def comment(self, ea, text):
        addr = self._addr(ea)
        self.program.getListing().setComment(addr, self.CommentType.EOL_COMMENT, text)
        f = self.program.getFunctionManager().getFunctionAt(addr)
        if f is not None and f.getEntryPoint() == addr:
            f.setComment(text)
        return {"ea": _hex(ea)}

    def set_type(self, ea, decl):
        from ghidra.app.cmd.function import ApplyFunctionSignatureCmd
        from ghidra.app.util.cparser.C import CParser
        from ghidra.program.model.data import FunctionDefinitionDataType
        from ghidra.program.model.symbol import SourceType

        dt = CParser(self.program.getDataTypeManager()).parse(decl)
        addr = self._addr(ea)
        f = self.program.getFunctionManager().getFunctionAt(addr)
        if f is not None and isinstance(dt, FunctionDefinitionDataType):
            ApplyFunctionSignatureCmd(addr, dt, SourceType.USER_DEFINED).applyTo(self.program, self.monitor)
        else:
            self.program.getListing().createData(addr, dt)
        return {"ea": _hex(ea), "type": str(dt)}

    def make_function(self, ea):
        from ghidra.app.cmd.function import CreateFunctionCmd

        addr = self._addr(ea)
        CreateFunctionCmd(addr).applyTo(self.program, self.monitor)
        f = self.program.getFunctionManager().getFunctionAt(addr)
        if f is None:
            raise RuntimeError(f"Ghidra could not make a function at {_hex(ea)}")
        return {"ea": str(f.getEntryPoint()), "name": f.getName()}

    def save(self):
        try:
            self.flat.saveProgram()
        except Exception:
            self.program.getDomainFile().save(self.monitor)
        return {"path": self.database}

    def close(self):
        try:
            self.decompiler.dispose()
        finally:
            with contextlib.suppress(Exception):
                self._cm.__exit__(None, None, None)

    def globals(self):
        return {
            "program": self.program, "currentProgram": self.program, "flat": self.flat,
            "monitor": self.monitor, "pyghidra": sys.modules.get("pyghidra"),
            "jpype": sys.modules.get("jpype"),
        }


class Binja(Adapter):
    label = "Binary Ninja"

    def __init__(self, binary, database, arch):
        super().__init__(binary, database, arch)
        import binaryninja

        self.bn = binaryninja
        self.bv = binaryninja.load(binary)
        if self.bv is None:
            raise RuntimeError(f"Binary Ninja could not load {binary}")
        self.bv.update_analysis_and_wait()
        self.format = self.bv.view_type or ""
        arch_name = self.bv.arch.name if self.bv.arch else ""
        self.arch_name = arch_name
        self.bits = self.bv.address_size * 8

    def _symbol(self, ea):
        return self.bv.get_symbol_at(ea)

    def functions(self):
        return [(int(f.start), f.symbol.name if f.symbol else f"sub_{f.start:x}") for f in self.bv.functions]

    def _function(self, ea):
        f = self.bv.get_function_at(ea)
        if f is None:
            raise KeyError(f"no function at {_hex(ea)}")
        return f

    def pseudocode(self, ea):
        f = self._function(ea)
        for attr in ("hlil", "mlil"):
            il = getattr(f, attr, None)
            if il is not None:
                return "\n".join(str(block) for block in il.root)
        raise RuntimeError("this Binary Ninja build exposes neither HLIL nor MLIL")

    def disassembly(self, ea):
        f = self._function(ea)
        out = []
        for block in f.basic_blocks:
            for line in self.bv.get_disassembly(block.start, block.end) or []:
                out.append(str(line))
        return "\n".join(out)

    def imports(self):
        out = []
        for sym in self.bv.get_symbols_of_type(self.bn.SymbolType.ImportSymbol):
            out.append((int(sym.address), sym.short_name or sym.name))
        return out

    def exports(self):
        out = []
        for name, addrs in (self.bv.exports or {}).items():
            for addr in addrs:
                out.append((int(addr), name))
        return out

    def symbol_address(self, name):
        syms = self.bv.get_symbols_by_name(name)
        return int(syms[0].address) if syms else None

    def strings(self):
        return [(int(s.address), s.value) for s in self.bv.strings]

    def xrefs_to(self, ea):
        return [{"from": _hex(r.address), "type": getattr(r, "type", "")} for r in self.bv.get_code_refs(ea)]

    def xrefs_from(self, ea):
        f = self.bv.get_function_at(ea)
        out = []
        if f is not None:
            for block in f.mlil.basic_blocks:
                for insn in block:
                    for target in getattr(insn, "dest", []) if isinstance(getattr(insn, "dest", None), list) else []:
                        out.append({"to": _hex(target), "type": "call"})
        return out

    def segments(self):
        out = []
        for seg in self.bv.segments:
            out.append({
                "name": seg.name or "",
                "start": int(seg.start),
                "end": int(seg.end),
                "perms": ("r" if seg.readable else "-") + ("w" if seg.writable else "-") + ("x" if seg.executable else "-"),
            })
        return out

    def entries(self):
        out = [(int(self.bv.entry_point), "entry")] if self.bv.entry_point else []
        out.extend(self.exports()[:200])
        return out

    def read_bytes(self, ea, size):
        return self.bv.read(ea, size) or b""

    def rename(self, ea, name):
        sym = self._symbol(ea)
        old = sym.short_name if sym else ""
        kind = sym.type if sym else self.bn.SymbolType.FunctionSymbol
        self.bv.define_user_symbol(self.bn.Symbol(kind, ea, name))
        return {"ea": _hex(ea), "old": old, "new": name}

    def comment(self, ea, text):
        f = self.bv.get_function_at(ea)
        if f is not None:
            f.set_comment_at(ea, text)
            if f.start == ea:
                f.comment = text
        else:
            self.bv.set_comment_at(ea, text)
        return {"ea": _hex(ea)}

    def set_type(self, ea, decl):
        type_, name = self.bv.parse_type_string(decl)
        self.bv.define_user_type(name or f"type_{ea:x}", type_)
        if type_ is not None and getattr(type_, "type_class", None) == self.bn.TypeClass.FunctionType:
            f = self.bv.get_function_at(ea)
            if f is not None:
                f.set_user_type(type_)
        return {"ea": _hex(ea), "type": str(type_)}

    def make_function(self, ea):
        f = self.bv.create_user_function(ea)
        if f is None:
            raise RuntimeError(f"Binary Ninja could not make a function at {_hex(ea)}")
        return {"ea": _hex(ea), "name": f.symbol.name if f.symbol else ""}

    def save(self):
        self.bv.save()
        return {"path": self.database}

    def close(self):
        with contextlib.suppress(Exception):
            self.bv.file.close()

    def globals(self):
        return {"bv": self.bv, "binaryninja": self.bn, "wait_analysis": self.bv.update_analysis_and_wait}


BACKENDS = {"ida": Ida, "ghidra": Ghidra, "binja": Binja}


def _format_view(db, kind, target):
    if kind == "overview":
        segs = db.segments()
        entries = db.entries()
        funcs = db.functions()
        info = db.info()
        head = [
            f"# {info['backend']} {info.get('format') or ''} {info.get('arch') or ''}".rstrip(),
            f"path: {db.binary}",
            f"bits: {info.get('bits')}  segments: {len(segs)}  entry points: {len(entries)}  functions: {len(funcs)}",
            "segments:",
        ]
        head += [f"  {_hex(s['start'])}-{_hex(s['end'])}  {s['perms']}  {s['name']}" for s in segs]
        head.append("entry points:")
        head += [f"  {_hex(ea)}  {name}" for ea, name in entries[:MAX_LINES]]
        head.append("functions:")
        head += [f"  {_hex(ea)}  {name}" for ea, name in funcs[:MAX_LINES]]
        if len(funcs) > MAX_LINES:
            head.append(f"... ({len(funcs) - MAX_LINES} more; :<func|0xaddress> for one)")
        return "\n".join(head)

    if kind in ("imports", "exports", "strings"):
        rows = db.imports() if kind == "imports" else db.exports() if kind == "exports" else db.strings()
        shown = [f"{_hex(ea)}  {name}" for ea, name in rows]
        return _lines(shown, f" in {kind}")

    if kind in ("pseudocode", "asm"):
        ea = db.resolve(target)
        ea, name = db.function_at(ea)
        body = db.pseudocode(ea) if kind == "pseudocode" else db.disassembly(ea)
        head = [f"# {name} @ {_hex(ea)} ({'pseudocode' if kind == 'pseudocode' else 'assembly'})"]
        return "\n".join(head) + "\n" + body

    if kind == "xrefs":
        ea = db.resolve(target)
        rows = db.xrefs_to(ea)
        return _lines([f"{r.get('from') or r.get('to')}  {r.get('type', '')}" for r in rows], "") or f"no references to {_hex(ea)}"

    raise ValueError(f"unknown view {kind!r}")


def _serve(adapter):
    stdout = sys.stdout

    def reply(payload):
        stdout.write(json.dumps(payload) + "\n")
        stdout.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        rid = None
        try:
            request = json.loads(line)
            rid = request.get("id")
            method = request.get("method")
            params = request.get("params") or {}
            if method == "view":
                result = _format_view(adapter, params.get("kind", "overview"), params.get("target"))
            elif method == "exec":
                scope = {"__name__": "__dsm__", "db": adapter, "dsm": adapter}
                scope.update(adapter.globals())
                scope.update(_exec_helpers(adapter))
                buffer = io.StringIO()
                value = None
                error = None
                try:
                    with contextlib.redirect_stdout(buffer):
                        try:
                            value = repr(eval(compile(params["code"], "<dsm>", "eval"), scope))
                        except SyntaxError:
                            exec(compile(params["code"], "<dsm>", "exec"), scope)
                except BaseException as exc:  # SystemExit and KeyboardInterrupt too
                    error = "".join(traceback.format_exception_only(type(exc), exc)).strip()
                result = {"output": buffer.getvalue(), "value": value, "error": error}
            elif method == "rename":
                result = adapter.rename(adapter.resolve(params["target"]), params["name"])
            elif method == "comment":
                result = adapter.comment(adapter.resolve(params["target"]), params["text"])
            elif method == "set_type":
                result = adapter.set_type(adapter.resolve(params["target"]), params["decl"])
            elif method == "make_function":
                result = adapter.make_function(adapter.resolve(params["target"]))
            elif method == "save":
                result = adapter.save()
            elif method == "close":
                result = None
                if params.get("save"):
                    result = adapter.save()
                reply({"id": rid, "ok": True, "result": result})
                adapter.close()
                return
            else:
                raise ValueError(f"unknown method {method!r}")
            reply({"id": rid, "ok": True, "result": result})
        except BaseException as exc:
            reply({"id": rid, "ok": False, "error": "".join(traceback.format_exception_only(type(exc), exc)).strip()})


def _exec_helpers(db):
    def resolve(target):
        return db.resolve(target)

    def func(target):
        return db.function_at(db.resolve(target))[0]

    def name_of(ea):
        return db.function_at(db.resolve(ea))[1]

    def xrefs_to(target):
        return db.xrefs_to(db.resolve(target))

    def xrefs_from(target):
        return db.xrefs_from(db.resolve(target))

    def callers(target):
        return [name_of(r.get("from")) if _is_address(r.get("from")) else str(r.get("from")) for r in xrefs_to(target)]

    def callees(target):
        return [str(r.get("to")) for r in xrefs_from(target)]

    def filtered(rows, pattern, key):
        import re

        rx = re.compile(pattern, re.IGNORECASE) if pattern else None
        return tuple(row for row in rows if rx is None or rx.search(str(row[key])))

    def functions(pattern=None):
        return filtered([(ea, name) for ea, name in db.functions()], pattern, 1)

    def strings(pattern=None):
        return filtered([(ea, s) for ea, s in db.strings()], pattern, 1)

    def imports(pattern=None):
        return filtered([(ea, n) for ea, n in db.imports()], pattern, 1)

    def hexdump(target, size=64):
        ea = db.resolve(target)
        data = db.read_bytes(ea, size)
        rows = []
        for off in range(0, len(data), 16):
            chunk = data[off:off + 16]
            text = "".join(chr(b) if 32 <= b < 127 else "." for b in chunk)
            rows.append(f"{ea + off:08x}  {chunk.hex(' '):<47}  {text}")
        return "\n".join(rows)

    def read_bytes(target, size=64):
        ea = db.resolve(target)
        return db.read_bytes(ea, size)

    def rename(target, name):
        return db.rename(db.resolve(target), name)

    def comment(target, text, repeatable=False):
        return db.comment(db.resolve(target), text)

    def set_type(target, decl):
        return db.set_type(db.resolve(target), decl)

    def make_function(target):
        return db.make_function(db.resolve(target))

    def _is_address(value):
        return isinstance(value, str) and value.startswith("0x")

    return {
        "resolve": resolve, "func": func, "name_of": name_of,
        "xrefs_to": xrefs_to, "xrefs_from": xrefs_from, "callers": callers, "callees": callees,
        "functions": functions, "strings": strings, "imports": imports,
        "hexdump": hexdump, "read_bytes": read_bytes,
        "rename": rename, "comment": comment, "set_type": set_type, "make_function": make_function,
    }


def main():
    backend, binary, database, arch = sys.argv[1], sys.argv[2], sys.argv[3], (sys.argv[4] if len(sys.argv) > 4 else "")
    try:
        adapter = BACKENDS[backend](binary, database, arch)
    except BaseException as exc:
        sys.stdout.write(json.dumps({"ready": False, "error": f"{backend} could not open {binary}: {exc}"}) + "\n")
        sys.stdout.flush()
        return 1
    sys.stdout.write(json.dumps({"ready": True, "info": adapter.info()}) + "\n")
    sys.stdout.flush()
    _serve(adapter)
    return 0


if __name__ == "__main__":
    sys.exit(main())
