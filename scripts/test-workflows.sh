#!/usr/bin/env bash

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)

ruby - "$ROOT" <<'RUBY'
require "yaml"

root = ARGV.fetch(0)

def assert(condition, message)
  abort "FAIL: #{message}" unless condition
end

def workflow_events(workflow)
  workflow.key?("on") ? workflow.fetch("on") : workflow.fetch(true)
end

def each_node(value, &block)
  yield value
  case value
  when Hash
    value.each { |key, child| each_node(key, &block); each_node(child, &block) }
  when Array
    value.each { |child| each_node(child, &block) }
  end
end

def assert_uses_pinned(workflow, path)
  uses = []
  each_node(workflow) do |value|
    uses << value if value.is_a?(String) && value.include?("@") && value.match?(/\A[^@\s]+@/)
  end
  assert(!uses.empty?, "#{path} has no action references")
  uses.each do |ref|
    assert(ref.match?(/\A[^@\s]+@[0-9a-f]{40}\z/), "#{path} has an unpinned action: #{ref}")
  end
end

portability_path = File.join(root, ".github/workflows/portability-ci.yml")
unstable_path = File.join(root, ".github/workflows/unstable-release.yml")
portability_text = File.read(portability_path)
unstable_text = File.read(unstable_path)
portability = YAML.load_file(portability_path)
unstable = YAML.load_file(unstable_path)

portability_events = workflow_events(portability)
assert(portability_events.fetch("pull_request").fetch("branches") == ["devel"],
       "portability pull request trigger must target devel")
assert(portability_events.fetch("push").fetch("branches").include?("gh/**"),
       "portability push trigger must include gh/**")
assert(portability_events.key?("workflow_dispatch"),
       "portability workflow must support manual dispatch")
assert(portability.fetch("permissions") == {"contents" => "read", "pull-requests" => "read"},
       "portability permissions must remain read-only")
portability_job = portability.fetch("jobs").fetch("workspace")
assert(portability_job.fetch("name") == "${{ github.event_name }} / ${{ matrix.os }}",
       "portability job name must identify the event and runner")
assert(portability_text.include?('"sha256":'),
       "portability manifest must include the archive sha256 field")
assert(portability_text.include?("shasum -a 256") || portability_text.include?("sha256sum"),
       "portability manifest must compute an archive SHA-256 digest")
assert(portability_text.include?("if: ${{ success() }}") &&
       portability_text.include?("if-no-files-found: error"),
       "portability build upload must be success-only and require files")
assert(portability_text.include?("if: always()") && portability_text.include?("target-*.log"),
       "portability log upload must always include target logs")
assert(portability_text.include?("Windows") && portability_text.include?(".exe"),
       "portability packaging must have a Windows executable branch")
assert_uses_pinned(portability, portability_path)

events = workflow_events(unstable)
assert(events.fetch("push").fetch("branches") == ["devel"],
       "unstable push trigger must target devel")
assert(events.key?("workflow_dispatch"), "unstable workflow must support manual dispatch")
assert(unstable.fetch("permissions") == {}, "unstable workflow permissions must be empty")

jobs = unstable.fetch("jobs")
%w[prepare build].each do |job_name|
  assert(jobs.fetch(job_name).fetch("permissions") == {
    "contents" => "read",
    "pull-requests" => "read"
  }, "#{job_name} permissions must be contents/read and pull-requests/read")
end
prepare = jobs.fetch("prepare")
assert(prepare.fetch("runs-on") == "ubuntu-24.04",
       "prepare must use the pinned Ubuntu 24.04 runner")
prepare_checkout = prepare.fetch("steps").find do |step|
  step["uses"].to_s.start_with?("actions/checkout@")
end
assert(prepare_checkout, "prepare must checkout the repository")
assert(prepare_checkout.fetch("with").fetch("ref") == "devel",
       "prepare must explicitly checkout devel")
prepare_run = prepare.fetch("steps").find { |step| step["id"] == "resolve" }.fetch("run")
assert(prepare_run.include?("checked_out_sha=\"$(git rev-parse HEAD)\"") &&
       prepare_run.include?("[ \"$checked_out_sha\" != \"$candidate_sha\" ]"),
       "prepare must verify the resolved API SHA matches the devel checkout")
assert(prepare_run.include?("PUSH_SHA") &&
       prepare_run.include?("[ \"$EVENT_NAME\" = push ]"),
       "prepare must retain push SHA validation")
publish = jobs.fetch("publish")
assert(publish.fetch("runs-on") == "ubuntu-24.04",
       "publish must use the pinned Ubuntu 24.04 runner")
assert(publish.fetch("permissions") == {
  "contents" => "write",
  "pull-requests" => "read"
}, "publish permissions must be contents/write and pull-requests/read only")
assert(publish.fetch("needs") == ["prepare", "build"],
       "publish must need prepare and build")
assert(publish.fetch("if").include?("needs.prepare.result == 'success'") &&
       publish.fetch("if").include?("needs.build.result == 'success'") &&
       !publish.fetch("if").include?("always()"),
       "publish must have an explicit success gate without always()")
assert(!publish.to_s.include?("always()"),
       "publish must not contain an always() condition")
assert(publish.fetch("concurrency") == {
  "group" => "unstable-release",
  "cancel-in-progress" => false
}, "publish concurrency must serialize unstable releases without cancellation")

build_job = jobs.fetch("build")
assert(build_job.fetch("strategy").fetch("matrix").fetch("os").include?("ubuntu-24.04"),
       "unstable builds must use the pinned Ubuntu 24.04 runner")
assert(!unstable_text.include?("ubuntu-latest"),
       "unstable workflow must not use the migrating ubuntu-latest label")
build_text = build_job.to_s
candidate_ref = "${{ needs.prepare.outputs.candidate_sha }}"
%w[build publish].each do |job_name|
  job = jobs.fetch(job_name)
  checkout = job.fetch("steps").find do |step|
    step["uses"].to_s.start_with?("actions/checkout@")
  end
  assert(checkout, "#{job_name} must checkout the candidate")
  assert(checkout.fetch("with").fetch("ref") == candidate_ref,
         "#{job_name} must checkout the exact prepare candidate SHA")
  verify = job.fetch("steps").find { |step| step["name"] == "Verify candidate checkout" }
  assert(verify && verify.fetch("env").fetch("EXPECTED_SHA") == candidate_ref &&
         verify.fetch("run").include?("git rev-parse HEAD") &&
         verify.fetch("run").include?("test \"$actual_sha\" = \"$EXPECTED_SHA\""),
         "#{job_name} must verify checkout provenance")
end
build_upload = build_job.fetch("steps").find do |step|
  step["uses"].to_s.start_with?("actions/upload-artifact@")
end
assert(build_upload, "unstable build must upload an artifact")
assert(build_upload.fetch("if") == "${{ success() }}", "unstable build upload must be success-only")
assert(build_upload.fetch("with").fetch("if-no-files-found") == "error",
       "unstable build upload must require files")
assert(build_upload.fetch("with").fetch("name") == "unstable-build-${{ matrix.os }}",
       "unstable build artifact names must be runner-specific")
assert(build_text.include?("needs.prepare.outputs.candidate_sha") &&
       build_text.include?("git rev-parse HEAD") &&
       build_text.include?("scripts/check-public-tree.sh"),
       "unstable builds must use and verify the prepare SHA and public tree")
assert(build_text.include?('manifest-$runner.json') &&
       build_text.include?("sha256") &&
       build_text.include?(".tar.gz") && build_text.include?(".zip"),
       "unstable builds must package target archives and digest manifests")

publish_steps_text = publish.fetch("steps").map { |step| step["run"].to_s }.join("\n")
assert(publish_steps_text.include?("scripts/check-public-tree.sh ."),
       "publish must check public-tree safety before staging")
assert(publish_steps_text.include?("merge-multiple: true") ||
       publish.fetch("steps").any? { |step| step["with"].to_s.include?("merge-multiple") },
       "publish must flatten all matrix artifacts")
assert(publish_steps_text.include?("unexpected") || publish_steps_text.include?("unexpected file"),
       "publish must reject unexpected candidate files")
assert(publish_steps_text.include?("scripts/publish-unstable.sh") &&
       publish_steps_text.include?("--repo") && publish_steps_text.include?("--sha") &&
       publish_steps_text.include?("--assets-dir"),
       "publish must invoke the unstable publisher with its full contract")
%w[
  manifest-macos.json
  manifest-linux.json
  manifest-windows.json
  diskdrift-macos-aarch64-apple-darwin.tar.gz
  diskdrift-macos-x86_64-apple-darwin.tar.gz
  diskdrift-linux-x86_64-unknown-linux-gnu.tar.gz
  diskdrift-windows-x86_64-pc-windows-msvc.zip
].each do |name|
  assert(publish_steps_text.include?(name),
         "publish candidate staging must enumerate #{name}")
end
assert(publish_steps_text.include?('test "${#entries[@]}" -eq 6'),
       "publish candidate staging must require exactly six files")

publisher_path = File.join(root, "scripts/publish-unstable.sh")
publisher_text = File.read(publisher_path)
assert(publisher_text.include?('"$WORK_DIR/SHA256SUMS"') &&
       publisher_text.include?("hash_file \"$archive\"") &&
       publisher_text.include?("SHA256SUMS"),
       "publisher must generate and verify SHA256SUMS")

assert(unstable_text.include?("repos/$REPO/git/ref/heads/devel") ||
       unstable_text.include?("repos/${REPO}/git/ref/heads/devel"),
       "prepare must resolve the current devel head through the API")
assert(unstable_text.include?("merged_at") && unstable_text.include?("base.ref"),
       "prepare must require a merged PR targeting devel")
assert(prepare_run.include?("--slurp") &&
       prepare_run.include?("|\n  jq -r 'add | any("),
       "prepare must pipe paginated API JSON through jq instead of combining gh --slurp with --jq")
private_path_markers = ["CLAUDE.md", "AGENTS.md", "docs/superpowers", "specs/", "claude/"]
[portability_text, unstable_text].each do |workflow_text|
  private_path_markers.each do |marker|
    assert(!workflow_text.include?(marker),
           "workflow must not depend on private path #{marker}")
  end
end
assert_uses_pinned(unstable, unstable_path)

puts "workflow structural assertions passed"
RUBY
