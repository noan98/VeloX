#!/usr/bin/env python3
"""プロセスツリー全体のメモリを OS 非依存の形で採るための共通モジュール (Issue #197)。

なぜ共通化したのか
------------------
`compare_browsers.py` / `tab_scaling.py` / `tab_churn.py` はそれぞれ同じ
`/proc` 走査を持っていた (`scripts/profile/pss_sampler.py` も同型)。Windows 対応を
足すにあたって 4 か所目を増やすのは明らかに悪手なので、ここに 1 つだけ置く。

なぜ Windows 対応が要るのか
---------------------------
`docs/performance-targets.md` の T2 は「メモリで Chromium 比 +10% 以内」だが、
**この評価は Linux でしか行えていなかった** — 比較ハーネス (`compare_browsers.py`)
が `/proc` 前提だったためである。CLAUDE.md は Windows を最優先と定めており、
Windows で評価できない目標を Stage 1 (Issue #176) の完了条件に据えることはできない
(docs/decisions.md D97 Revisit condition (3))。

Windows に PSS は無い — だから「挟み込む」
------------------------------------------
Linux の `smaps_rollup` の `Pss:` は、共有ページを共有プロセス数で割った値を
**カーネルが計算して**返す。Windows にこれに相当するものは無い (D88 が調査済み)。

D88 は代替として `QueryWorkingSetEx` の `ShareCount` で `1/ShareCount` を足し上げる
近似を検討し、「正確さが自明でない」として見送った。**その判断は正しかった** —
`PSAPI_WORKING_SET_BLOCK` の `ShareCount` は **3 bit しかなく、7 で飽和する**。
ブラウザのように 8 個以上のプロセスが同じ DLL ページを共有する状況では、
共有ページの重みが実際より重く出る。しかも飽和の度合いはプロセス数に依存するので、
**プロセス構成の違うブラウザ同士の比較という、まさに使いたい用途で歪む。**

そこでこのモジュールは近似値を作らず、**真の PSS を上下から挟む厳密な値**を返す。
`QueryWorkingSet` は working set 中の各ページについて「共有されているか
(`Shared` ビット)」と「何プロセスで共有されているか (`ShareCount`)」を返すので、
1 回の呼び出しで両方が得られる。

    private_bytes  <=  真の PSS  <=  pss_upper_bytes  <=  rss_bytes

- **下界 `private_bytes`** (Private Working Set 合計): 共有ページを 1 つも
  数えない。共有ページの取り分は必ず 0 より大きいので、真の PSS はこれ以上。
- **上界 `pss_upper_bytes`**: 私有ページ + 共有ページを `ShareCount` で割った和。
  **ここが D88 との違いである。** D88 は `1/ShareCount` を**近似値**として使う
  ことを検討して見送ったが、**上界として使えば飽和は破綻しない** — 報告値を c、
  実際の共有プロセス数を n とすると、飽和していなければ n == c、飽和していれば
  n >= 7 == c なので、**どちらでも n >= c**。よって各ページの寄与
  `page_size / n <= page_size / c` であり、和は必ず真の PSS 以上になる。
  飽和は上界を緩めるだけで、上界であること自体を壊さない。
- **`rss_bytes`** (Working Set 合計) も正しい上界だが、「c = 1 と置いた」のと
  同じで最も緩い。実測では 4 倍ほど緩かった (§29.8)。

比較の判定にどう使うかは `compare_browsers.py` の `compare_bounds` を参照。
**区間が重なっている間は「判定不能」と言う** — 片方の代表値を選んで断定しない。

Linux 側は従来どおり `Pss:` をそのまま返す (`private_bytes` は `None`)。
Windows 側は `pss_bytes` が常に `None`。**OS をまたいで数値を比較してはならない**
(D88 の警告、および Epic #57 の絶対ルール5)。
"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass
from pathlib import Path

#: Windows の `PSAPI_WORKING_SET_BLOCK.ShareCount` は 3 bit で、7 で飽和する。
#: 近似 PSS を作らない理由そのものなので、定数として残して参照できるようにする。
WINDOWS_SHARE_COUNT_MAX = 7


@dataclass(frozen=True)
class TreeMemory:
    """プロセスツリー 1 本ぶんのメモリ計測結果。

    `pss_bytes` と `private_bytes` は**片方だけが埋まる** — Linux は前者、
    Windows は後者。両方 `None` なら、その OS では詳細が取れていない。
    """

    #: Linux: `statm` の resident 合計 / Windows: `WorkingSetSize` 合計。
    #: どちらも共有ページを重複して数えるため、**真の PSS の上界**である。
    rss_bytes: int
    #: Linux のみ。`smaps_rollup` の `Pss:` 合計 (カーネル計算値)。
    pss_bytes: int | None
    #: Windows のみ。Private Working Set 合計 = **真の PSS の下界**。
    private_bytes: int | None
    #: Windows のみ。`ShareCount` から導いた **真の PSS の上界** (下記参照)。
    #: Working Set 合計よりずっと締まっているので、こちらを上界に使う。
    pss_upper_bytes: int | None
    #: ツリーに含まれたプロセス数。
    process_count: int
    #: そのうち、メモリの詳細を取得できなかったプロセス数。0 でないときの
    #: 合計値は過小評価になっているので、結果を読むときに必ず添えること。
    unreadable_count: int

    @property
    def lower_bytes(self) -> int | None:
        """真の PSS の下界。Linux では PSS 自身 (下界かつ上界)。"""
        return self.pss_bytes if self.pss_bytes is not None else self.private_bytes

    @property
    def upper_bytes(self) -> int:
        """真の PSS の上界。

        優先順位は PSS 自身 (Linux) → `ShareCount` 由来の上界 (Windows) →
        Working Set 合計。最後のものは「共有ページを 1 プロセスでしか使って
        いないと仮定した」のと同じで、**上界としては正しいが極端に緩い。**
        """
        if self.pss_bytes is not None:
            return self.pss_bytes
        if self.pss_upper_bytes is not None:
            return self.pss_upper_bytes
        return self.rss_bytes


def collect_tree(
    root_pid: int, nodes: dict[int, tuple[int, "_ProcMemory | None"]]
) -> TreeMemory:
    """PID → (親 PID, メモリ) の表から、`root_pid` を根とする部分木を集計する。

    OS 依存の収集 (`/proc` 走査 / Toolhelp32 スナップショット) と切り離してある
    ので、**この関数だけは全 OS で単体テストできる。** Windows 実機を持たない
    環境で回帰を止められるのはここまでなので、木の走査ロジックは全部ここに置く。

    親 PID が再利用されて循環ができても止まるよう、訪問済みを記録しながら回る。
    """
    children: dict[int, list[int]] = {}
    for pid, (ppid, _) in nodes.items():
        children.setdefault(ppid, []).append(pid)

    rss_total = 0
    pss_total = 0
    private_total = 0
    pss_upper_total = 0
    has_pss = False
    has_private = False
    has_pss_upper = False
    count = 0
    unreadable = 0

    stack = [root_pid]
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen or pid not in nodes:
            continue
        seen.add(pid)
        count += 1
        mem = nodes[pid][1]
        if mem is None:
            unreadable += 1
        else:
            rss_total += mem.rss_bytes
            if mem.pss_bytes is not None:
                pss_total += mem.pss_bytes
                has_pss = True
            if mem.private_bytes is not None:
                private_total += mem.private_bytes
                has_private = True
            if mem.pss_upper_bytes is not None:
                pss_upper_total += mem.pss_upper_bytes
                has_pss_upper = True
        stack.extend(children.get(pid, []))

    return TreeMemory(
        rss_bytes=rss_total,
        pss_bytes=pss_total if has_pss else None,
        private_bytes=private_total if has_private else None,
        pss_upper_bytes=pss_upper_total if has_pss_upper else None,
        process_count=count,
        unreadable_count=unreadable,
    )


def working_set_pages(blocks, page_size: int) -> tuple[int, int]:
    """`QueryWorkingSet` のブロック列から (Private Working Set, PSS の上界)。

    **`PSAPI_WORKING_SET_BLOCK` のビット配置**: Protection:5, ShareCount:3,
    Shared:1, ... したがって `Shared` は bit 8、`ShareCount` は bit 5-7。

    共有ページを `ShareCount` で割るのは**近似のためではなく、上界を締める
    ためである** (理由は `working_set_breakdown` の説明を参照)。

    FFI の外に切り出してあるのは、**Windows 実機なしでこの計算をテストできる
    ようにするため。** run 34364659960 の教訓 (計測は成功していたのに、検証の
    無い箇所で落ちた) を踏まえている。
    """
    private_pages = 0
    # 共有ページの寄与は 1/c ずつ足すので端数が出る。先に page_size を
    # 掛けず、重みの合計を持ってから掛ける。
    shared_weight = 0.0
    for block in blocks:
        if (block >> 8) & 1:
            share_count = (block >> 5) & 0x7
            # 共有ページで c == 0 は本来ありえないが 0 除算を避ける。
            # 1 とみなしても上界であることは保たれる。
            shared_weight += 1.0 / max(share_count, 1)
        else:
            private_pages += 1
    private_bytes = private_pages * page_size
    return private_bytes, private_bytes + int(shared_weight * page_size)


@dataclass(frozen=True)
class _ProcMemory:
    """1 プロセスぶんのメモリ。`collect_tree` の入力。"""

    rss_bytes: int
    pss_bytes: int | None = None
    private_bytes: int | None = None
    pss_upper_bytes: int | None = None


# --------------------------------------------------------------------------
# Linux (`/proc`)
# --------------------------------------------------------------------------


def _linux_pss_bytes(entry: Path) -> int | None:
    """`smaps_rollup` の `Pss:` 行 (kB 単位)。読めなければ `None`。"""
    try:
        for line in (entry / "smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                return int(line.split()[1]) * 1024
    except (OSError, IndexError, ValueError):
        return None
    return None


def _linux_nodes() -> dict[int, tuple[int, _ProcMemory | None]]:
    page_size = os.sysconf("SC_PAGE_SIZE")
    nodes: dict[int, tuple[int, _ProcMemory | None]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            # `stat` の comm はスペースや括弧を含みうるので、最後の ") " で切る。
            fields = (entry / "stat").read_text().rsplit(") ", 1)[1].split()
            ppid = int(fields[1])
            resident_pages = int((entry / "statm").read_text().split()[1])
        except (OSError, IndexError, ValueError):
            # 走査中に終了したプロセス。ツリーから落ちるだけで失敗にはしない。
            continue
        nodes[pid] = (
            ppid,
            _ProcMemory(
                rss_bytes=resident_pages * page_size,
                pss_bytes=_linux_pss_bytes(entry),
            ),
        )
    return nodes


# --------------------------------------------------------------------------
# Windows (Toolhelp32 + PSAPI)
# --------------------------------------------------------------------------


def _windows_nodes() -> dict[int, tuple[int, _ProcMemory | None]]:
    """Toolhelp32 でツリーを、`QueryWorkingSet` でページ内訳を採る。

    プロセス一覧の取り方は `browser::metrics` の Windows 実装 (D88) と同じ
    `CreateToolhelp32Snapshot` にそろえてある — 同じ木を見ていることを担保する
    ため。RSS も同じ `GetProcessMemoryInfo` の `WorkingSetSize` なので、
    ここで出る `rss_bytes` は `velox-bench` の `rss_total_bytes` と同じ定義
    (docs/performance-targets.md §28 の数値と並べて読める)。

    `QueryWorkingSet` は追加で「working set の各ページが共有か否か」を返す。
    共有でないページ数 × ページサイズが Private Working Set = 真の PSS の下界。
    """
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)

    # ⚠️ **`restype` を明示しないと 64bit Windows で壊れる。** ctypes の既定の
    # 戻り値型は C の `int` (32bit) なので、`OpenProcess` /
    # `CreateToolhelp32Snapshot` が返す 64bit の HANDLE が**上位 32bit を
    # 落として**返ってくる。切り詰められたハンドルは無効か、最悪は別のオブジェクト
    # を指す。同じ理由で、ハンドルを受け取る側にも `argtypes` を与える。
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    kernel32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel32.CloseHandle.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    psapi.GetProcessMemoryInfo.argtypes = [
        wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD
    ]
    psapi.QueryWorkingSet.restype = wintypes.BOOL
    psapi.QueryWorkingSet.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD]

    TH32CS_SNAPPROCESS = 0x00000002
    INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value
    ERROR_BAD_LENGTH = 24
    # `QueryWorkingSet` は PROCESS_QUERY_INFORMATION を要求する
    # (PROCESS_QUERY_LIMITED_INFORMATION では足りない)。取れなければ
    # LIMITED に落として、RSS だけでも拾う。
    PROCESS_QUERY_INFORMATION = 0x0400
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    PROCESS_VM_READ = 0x0010

    class PROCESSENTRY32W(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD),
            ("cntUsage", wintypes.DWORD),
            ("th32ProcessID", wintypes.DWORD),
            ("th32DefaultHeapID", ctypes.c_size_t),
            ("th32ModuleID", wintypes.DWORD),
            ("cntThreads", wintypes.DWORD),
            ("th32ParentProcessID", wintypes.DWORD),
            ("pcPriClassBase", ctypes.c_long),
            ("dwFlags", wintypes.DWORD),
            ("szExeFile", wintypes.WCHAR * 260),
        ]

    class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
        _fields_ = [
            ("cb", wintypes.DWORD),
            ("PageFaultCount", wintypes.DWORD),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    kernel32.Process32FirstW.restype = wintypes.BOOL
    kernel32.Process32FirstW.argtypes = [wintypes.HANDLE, ctypes.c_void_p]
    kernel32.Process32NextW.restype = wintypes.BOOL
    kernel32.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.c_void_p]

    page_size = _windows_page_size(kernel32)

    def open_process(pid: int):
        for access in (
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
        ):
            handle = kernel32.OpenProcess(access, False, pid)
            if handle:
                return handle
        return None

    def working_set_bytes(handle) -> int | None:
        counters = PROCESS_MEMORY_COUNTERS()
        counters.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS)
        ok = psapi.GetProcessMemoryInfo(
            handle, ctypes.byref(counters), counters.cb
        )
        return int(counters.WorkingSetSize) if ok else None

    def working_set_breakdown(handle) -> tuple[int, int] | None:
        """`QueryWorkingSet` から (Private Working Set, 真の PSS の上界) を返す。

        ページ単位の計算は `working_set_pages` に切り出してある — **FFI の外に
        置いて Windows 実機なしでテストできるようにするため** (§29.7 の教訓)。
        ここが担うのはバッファ確保と再取得だけである。

        バッファ長は事前に分からないので、`ERROR_BAD_LENGTH` で返ってきた
        `NumberOfEntries` を見て採り直す。**採り直しの間にもページは増減する**
        ので余裕を持たせ、それでも足りなければ諦めて `None` を返す (過小評価
        した値を黙って返すより、取れなかったと言う方がよい)。
        """
        entries = 4096
        for _ in range(4):
            # 先頭が NumberOfEntries、その後ろに entries 個のブロックが並ぶ。
            buf = (ctypes.c_size_t * (entries + 1))()
            size = ctypes.sizeof(buf)
            if psapi.QueryWorkingSet(handle, ctypes.byref(buf), size):
                actual = int(buf[0])
                if actual > entries:  # 想定外。数え漏らすくらいなら諦める
                    return None
                return working_set_pages(buf[1 : actual + 1], page_size)
            if ctypes.get_last_error() != ERROR_BAD_LENGTH:
                return None
            # 失敗時でも先頭には必要なエントリ数が書かれている。5 割ほど
            # 余裕を足して採り直す。
            needed = int(buf[0]) if buf[0] else entries * 2
            entries = max(entries * 2, needed + needed // 2 + 1024)
        return None

    nodes: dict[int, tuple[int, _ProcMemory | None]] = {}
    snapshot = kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    # restype が HANDLE (= c_void_p) なので、返り値は int か None。
    # 失敗時は INVALID_HANDLE_VALUE (-1 の符号なし表現) が返る。
    if not snapshot or snapshot == INVALID_HANDLE_VALUE:
        raise OSError(ctypes.get_last_error(), "CreateToolhelp32Snapshot に失敗しました")
    try:
        entry = PROCESSENTRY32W()
        entry.dwSize = ctypes.sizeof(PROCESSENTRY32W)
        more = kernel32.Process32FirstW(snapshot, ctypes.byref(entry))
        while more:
            pid = int(entry.th32ProcessID)
            ppid = int(entry.th32ParentProcessID)
            handle = open_process(pid)
            if handle is None:
                # 権限が足りないプロセス。ツリーには残すが値は持たない
                # (Linux 側で `/proc/<pid>` が読めない場合と同じ扱い、D88)。
                nodes[pid] = (ppid, None)
            else:
                try:
                    rss = working_set_bytes(handle)
                    breakdown = working_set_breakdown(handle)
                    nodes[pid] = (
                        ppid,
                        None
                        if rss is None
                        else _ProcMemory(
                            rss_bytes=rss,
                            private_bytes=None if breakdown is None else breakdown[0],
                            pss_upper_bytes=(
                                None if breakdown is None else breakdown[1]
                            ),
                        ),
                    )
                finally:
                    kernel32.CloseHandle(handle)
            more = kernel32.Process32NextW(snapshot, ctypes.byref(entry))
    finally:
        kernel32.CloseHandle(snapshot)
    return nodes


def _windows_page_size(kernel32) -> int:
    import ctypes
    from ctypes import wintypes

    class SYSTEM_INFO(ctypes.Structure):
        _fields_ = [
            ("wProcessorArchitecture", wintypes.WORD),
            ("wReserved", wintypes.WORD),
            ("dwPageSize", wintypes.DWORD),
            ("lpMinimumApplicationAddress", ctypes.c_void_p),
            ("lpMaximumApplicationAddress", ctypes.c_void_p),
            ("dwActiveProcessorMask", ctypes.c_size_t),
            ("dwNumberOfProcessors", wintypes.DWORD),
            ("dwProcessorType", wintypes.DWORD),
            ("dwAllocationGranularity", wintypes.DWORD),
            ("wProcessorLevel", wintypes.WORD),
            ("wProcessorRevision", wintypes.WORD),
        ]

    info = SYSTEM_INFO()
    kernel32.GetSystemInfo(ctypes.byref(info))
    return int(info.dwPageSize) or 4096


# --------------------------------------------------------------------------
# 公開 API
# --------------------------------------------------------------------------


def supported() -> bool:
    """この OS でプロセスツリーのメモリを採れるか。"""
    return sys.platform.startswith("linux") or sys.platform == "win32"


def process_tree_memory(root_pid: int) -> TreeMemory:
    """`root_pid` を根とするプロセスツリー全体のメモリ。

    ブラウザはマルチプロセスなので、親プロセスだけを見ても意味が無い
    (`browser::metrics::sample_process_tree_rss` と同じ考え方、D16)。
    """
    if sys.platform.startswith("linux"):
        nodes = _linux_nodes()
    elif sys.platform == "win32":
        nodes = _windows_nodes()
    else:
        raise NotImplementedError(
            f"{sys.platform} ではプロセスツリーのメモリを採れません "
            "(Linux の /proc と Windows の Toolhelp32 のみ対応)"
        )
    return collect_tree(root_pid, nodes)
