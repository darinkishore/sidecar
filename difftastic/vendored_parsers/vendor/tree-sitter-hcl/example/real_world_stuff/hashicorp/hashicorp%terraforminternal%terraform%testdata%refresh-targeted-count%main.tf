resource "aws_vpc" "metoo" {}
resource "aws_instance" "notme" { }
resource "aws_instance" "me" {
  vpc_id = "${aws_vpc.metoo.id}"
  count = 3
}
resource "aws_elb" "meneither" {
  instances = ["${aws_instance.me.*.id}"]
}
