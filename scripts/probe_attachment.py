"""Diagnose attachment flags using blank RAM disks, never payload files."""

import json
import re
import subprocess
import time
import uuid

for variant, options in [("baseline", []), ("kernel", ["-kernel"]),
                         ("lean", ["-noverify", "-noautofsck"])]:
    for sample in range(3):
        command = ["/usr/bin/hdiutil", "attach", "-nomount", *options,
                   "-drivekey", f"FileLeaseIdentifier={uuid.uuid4()}", "ram://131072"]
        start = time.perf_counter()
        # Capture only this command's device names and errors. Never enumerate
        # or detach pre-existing devices; every device below came from our own
        # successful attachment output. No payload is written to these volumes.
        result = subprocess.run(command, capture_output=True, text=True, timeout=20)
        elapsed = (time.perf_counter() - start) * 1000
        devices = re.findall(r"^(/dev/disk[0-9]+)\s", result.stdout, re.MULTILINE)
        print(json.dumps(dict(variant=variant, sample=sample, elapsed_ms=elapsed,
                              status=result.returncode, stdout=result.stdout,
                              stderr=result.stderr)), flush=True)
        for device in sorted(set(devices)):
            subprocess.run(["/usr/bin/hdiutil", "detach", "-force", device],
                           check=True, capture_output=True, timeout=20)
