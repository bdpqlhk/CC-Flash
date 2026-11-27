#!/usr/bin/env python3
"""publish job 用: 确定性维护 Release 正文的"未签名/未公证"说明块。

- 先删除历次自动追加的旧块(--- 引导、首行含"未签名/未公证构建"的连续引用行),
  再写入现行文案 —— 关键词 grep 跳过会把旧版错误文案永久保留(审查 B3-3)。
- 用法: python3 scripts/append-unsigned-notes.py <notes.md>
"""
import re
import sys

path = sys.argv[1]
s = open(path, encoding="utf-8").read()
pat = re.compile(r"\n*---\n+\n*> ?未签名/未公证构建[^\n]*\n(?:> ?[^\n]*\n?)*")
s = pat.sub("\n", s).rstrip("\n")
s += "\n\n---\n\n"
s += "> 未签名/未公证构建：macOS 首次打开请右键 → 打开；Windows 若出现 SmartScreen，请选择“更多信息 → 仍要运行”。\n"
s += "> 此版本标记为 prerelease：更新检查（自动与手动）仍会显示此版本并标注未签名/预发布风险，请自行确认后再下载。\n"
open(path, "w", encoding="utf-8").write(s)
