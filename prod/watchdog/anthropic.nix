# The watchdog's Anthropic identity: which organization, workspace, service
# account and federation rule its IAM role maps to. Created once in the Claude
# Console (Settings > Workload identity > Connect workload > AWS) with
#   issuer         https://a1df8c6e-784f-4651-977c-5b4ed5d7ab72.tokens.sts.global.api.aws
#   subject prefix arn:aws:iam::109251245574:role/caos-watchdog
#   audience       https://api.anthropic.com
# None of these is a secret: the credential is the role, asserted by an
# AWS-signed token the box mints at run time.
{
  organizationId = "TODO";
  workspaceId = "TODO";
  serviceAccountId = "TODO";
  federationRuleId = "TODO";
}
