resource "aws_instance" "ok" {
  ami = "x"
}

resource "aws_instance" "broken" {
  ami =
