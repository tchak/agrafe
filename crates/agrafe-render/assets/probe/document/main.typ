// The startup probe's template: one of each thing a template does, so a
// helper that cannot read data, include a file or place an image fails
// the deployment rather than every document.
#import "lib/header.typ": header
#set page(width: 10cm, height: 5cm, margin: 8mm)
#let d = json("data.json")
#header(d.title)
Built by #d.by on #datetime.today().display().
#image("pixel.png", width: 4mm)
