"""重新绘制应用图标：白色圆角矩形外框 + 缩小的蓝色圆角方块与暂停条"""
from PIL import Image, ImageDraw

S = 1024
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

BLUE = (41, 121, 245, 255)

# 外层：白色圆角矩形外框（实心）
fm = 40                      # 外框边距
fbox = [fm, fm, S - fm, S - fm]
fr = 200                     # 外框圆角半径
d.rounded_rectangle(fbox, radius=fr, fill=(255, 255, 255, 255))

# 内层：蓝色圆角方块（占画布约 3/5）
bs = 614
bx0 = (S - bs) // 2
bbox = [bx0, bx0, bx0 + bs, bx0 + bs]
br = int(bs * 0.28)          # 与原图标圆角比例一致
d.rounded_rectangle(bbox, radius=br, fill=BLUE)

# 暂停条：白色胶囊形，随蓝块等比缩放并居中
bw = int(bs * 0.09)          # 条宽
bh = int(bs * 0.34)          # 条高
gap = int(bs * 0.20)         # 两条中心间距
cy = S // 2
for cx in (S // 2 - gap // 2, S // 2 + gap // 2):
    x0, x1 = cx - bw // 2, cx + bw // 2
    y0, y1 = cy - bh // 2, cy + bh // 2
    d.rounded_rectangle([x0, y0, x1, y1], radius=bw // 2, fill=(255, 255, 255, 255))

img.save("app-icon.png")
print("done", img.size)
