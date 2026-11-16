resource "aws_iam_policy" "p" {
  name   = "policy"
  policy = <<-EOT
    {
      "Name": "${var.name}",
      "Arn": "${aws_iam_role.r.arn}",
      "Note": "not a { brace"
    }
  EOT
}

resource "aws_iam_role" "r" {
  name = "role"
}

variable "name" {}
