import re

filename = "README.md"

with open(filename, 'r', encoding='utf-8') as f:
    content = f.read()

# Find headers
headers = re.findall(r'^(#{2,6})\s+(.+)', content, re.MULTILINE)
headers = [h for h in headers if h[1].lower() != "table of contents"]

toc_items = []
for level, title in headers:
    # create link
    anchor = re.sub(r'[^\w\s-]', '', title.lower()).replace(' ', '-')
    indent = "  " * (len(level) - 2)
    toc_items.append(f"{indent}- [{title}](#{anchor})")

new_toc_block = '<!--tocbeg-->\n' + "## Table of Contents\n" + "\n".join(toc_items) + "\n" + '<!--tocend-->\n'
pattern = r'<!--\s*tocbeg\s*-->(.*?)<!--\s*tocend\s*-->'

if re.search(pattern, content, re.DOTALL):
    updated_content = re.sub(pattern, new_toc_block, content, flags=re.DOTALL)
else:
    raise ValueError("No marker")

with open(filename, 'w', encoding='utf-8') as f:
    f.write(updated_content)
