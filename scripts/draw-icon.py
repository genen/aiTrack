"""Draws the 1024 px app icon (assets/icon/aitrack-1024.png) from geometry, supersampled 4x. Build helper only."""
import math, sys
from PIL import Image, ImageDraw
S=4; N=1024*S
NAVY=(11,27,56,255); OR=(255,124,18,255); WH=(255,255,255,255); TE=(14,184,176,255); RING=(48,66,104,255)
def P(v): return v*2*S
img=Image.new("RGBA",(N,N),(0,0,0,0))
bg=Image.new("RGBA",(N,N),NAVY); m=Image.new("L",(N,N),0)
ImageDraw.Draw(m).rounded_rectangle([0,0,N-1,N-1],radius=P(92),fill=255); img.paste(bg,(0,0),m)
d=ImageDraw.Draw(img)
cx,cy=256,286
def arc(r0,r1,a0,a1,col):
    pts=[];n=160
    for i in range(n+1):
        a=math.radians(a0+(a1-a0)*i/n); pts.append((cx+r1*math.cos(a),cy-r1*math.sin(a)))
    for i in range(n+1):
        a=math.radians(a1+(a0-a1)*i/n); pts.append((cx+r0*math.cos(a),cy-r0*math.sin(a)))
    d.polygon([(P(x),P(y)) for x,y in pts],fill=col)
def cap(r,a,w,col):
    x,y=cx+r*math.cos(math.radians(a)),cy-r*math.sin(math.radians(a))
    d.ellipse([P(x-w),P(y-w),P(x+w),P(y+w)],fill=col)
R0,R1=156,194; mid=(R0+R1)/2; w=(R1-R0)/2
arc(R0,R1,203,126,OR); cap(mid,203,w,OR)
arc(R0,R1,123,57,WH)
arc(R0,R1,54,-23,TE); cap(mid,-23,w,TE)
cy=300
# thin dial ring
d.ellipse([P(cx-150),P(cy-150),P(cx+150),P(cy+150)],outline=RING,width=round(P(3.2)))
# ticks along the bottom
for a in (-125,-107,-90,-73,-55):
    r0,r1=118,134
    x0,y0=cx+r0*math.cos(math.radians(a)),cy-r0*math.sin(math.radians(a))
    x1,y1=cx+r1*math.cos(math.radians(a)),cy-r1*math.sin(math.radians(a))
    d.line([(P(x0),P(y0)),(P(x1),P(y1))],fill=RING,width=round(P(3.2)))
# rays above-left of hub
hx,hy=252,298
for a,r0,r1 in ((160,34,52),(125,30,50),(90,32,52),(55,34,50)):
    x0,y0=hx+r0*math.cos(math.radians(a)),hy-r0*math.sin(math.radians(a))
    x1,y1=hx+r1*math.cos(math.radians(a)),hy-r1*math.sin(math.radians(a))
    d.line([(P(x0),P(y0)),(P(x1),P(y1))],fill=WH,width=round(P(4.6)))
    for x,y in ((x0,y0),(x1,y1)): d.ellipse([P(x-2.3),P(y-2.3),P(x+2.3),P(y+2.3)],fill=WH)
# short tail dash right of hub
d.line([(P(296),P(299)),(P(314),P(299))],fill=WH,width=round(P(4.6)))
# needle: tapered polygon from hub toward upper right
tipx,tipy=372,213
ang=math.atan2(-(tipy-hy),tipx-hx); px,py=-math.sin(ang),-math.cos(ang)
base=10
d.polygon([(P(hx+0*math.cos(ang)+px*base),P(hy-(0*math.sin(ang))+py*-base*-1+0)) ,(P(tipx),P(tipy)),(P(hx-px*base),P(hy+py*base*-1))],fill=WH) if False else None
nx,ny=-(tipy-hy),(tipx-hx); L=math.hypot(nx,ny); nx/=L; ny/=L
d.polygon([(P(hx+nx*11),P(hy+ny*11)),(P(tipx),P(tipy)),(P(hx-nx*11),P(hy-ny*11))],fill=WH)
# hub: white ring with navy centre
d.ellipse([P(hx-19),P(hy-19),P(hx+19),P(hy+19)],fill=WH)
d.ellipse([P(hx-11.5),P(hy-11.5),P(hx+11.5),P(hy+11.5)],fill=NAVY)
img.resize((1024,1024),Image.LANCZOS).save(sys.argv[1] if len(sys.argv)>1 else "assets/icon/aitrack-1024.png")

