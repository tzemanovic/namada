from ghapi.all import GhApi
from os import environ
from json import loads, load
from tempfile import gettempdir
import subprocess
import re


def download_artifact(url: str, path: str, zip_name: str, token: str):
    return subprocess.run(["curl", "-s", "-H", "Accept: application/vnd.github+json".format(token), "-H", "Authorization: token {}".format(token), url, "-L", "-o", "{}/{}.zip".format(path, zip_name)], capture_output=True)


def unzip(path: str, zip_name: str):
    return subprocess.run(["unzip", "-o", "{}/{}.zip".format(path, zip_name), "-d", path], capture_output=True)


def publish_wasm(path: str, file_name: str, bucket: str):
    return subprocess.run(["aws", "s3", "cp", "{}/{}".format(path, file_name), "s3://{}".format(bucket), "--acl", "public-read"], capture_output=True)

def download_genesis_template(repository_owner:str, template_name: str, to: str):
    url = "https://raw.githubusercontent.com/{}/anoma-network-config/master/templates/{}.toml".format(repository_owner, template_name)
    return subprocess.run(["curl", "-s", url, "-o", "{}/template.toml".format(to)])
    

def generate_genesis_template(binary_path: str, chain_prefix: str, folder: str):
    command = "{}/namadac utils init-network --chain-prefix {} --genesis-path {}/template.toml --consensus-timeout-commit 10s --wasm-checksums-path {}/checksums.json --unsafe-dont-encrypt --allow-duplicate-ip".format(binary_path, chain_prefix, folder, folder)
    return subprocess.run(command.split(" "), capture_output=True)

def log(data: str):
    print(data)


TOKEN = environ["GITHUB_TOKEN"]
READ_ORG_TOKEN = environ['GITHUB_READ_ORG_TOKEN']
REPOSITORY_OWNER = environ['GITHUB_REPOSITORY_OWNER']
TMP_DIRECTORY = gettempdir()
ARTIFACT_PER_PAGE = 75
WASM_BUCKET = 'namada-wasm-master'

read_org_api = GhApi(token=READ_ORG_TOKEN)
api = GhApi(owner=REPOSITORY_OWNER, repo="namada", token=TOKEN)

comment_event = loads(environ['GITHUB_CONTEXT'])

user_membership = read_org_api.teams.get_membership_for_user_in_org(
    'heliaxdev', 'company', comment_event['event']['sender']['login'])
if user_membership['state'] != 'active':
    exit(0)

pr_comment = comment_event['event']['comment']['body']
pr_number = comment_event['event']['issue']['number']

pr_info = api.pulls.get(pr_number)
head_sha = pr_info['head']['sha']
short_sha = head_sha[0:7]

parameters = re.search('\[([^\]]+)', pr_comment).group(1).split(', ')
template_name = parameters[0]

artifacts = api.actions.list_artifacts_for_repo(per_page=ARTIFACT_PER_PAGE)
steps_done = 0

log("Downloading artifacts...")

for artifact in artifacts['artifacts']:
    if 'wasm' in artifact['name'] and artifact['workflow_run']['head_sha'] == head_sha and not artifact['expired']:
        artifact_download_url = artifact['archive_download_url']

        curl_command_outcome = download_artifact(
            artifact_download_url, TMP_DIRECTORY, "wasm", TOKEN)
        if curl_command_outcome.returncode != 0:
            exit(1)

        log("Unzipping wasm.zip...")
        unzip_command_outcome = unzip(TMP_DIRECTORY, "wasm")
        if unzip_command_outcome.returncode != 0:
            exit(1)

        checksums = load(open("{}/checksums.json".format(TMP_DIRECTORY)))
        for wasm in checksums.values():
            log("Uploading {}...".format(wasm))
            publish_wasm_command_outcome = publish_wasm(
                TMP_DIRECTORY, wasm, WASM_BUCKET)
            if publish_wasm_command_outcome.returncode != 0:
                print("Error uploading {}!".format(wasm))

        steps_done += 1
        log("Done wasm!")
    
    elif 'binaries' in artifact['name'] and artifact['workflow_run']['head_sha'] == head_sha and not artifact['expired']:
        artifact_download_url = artifact['archive_download_url']

        curl_command_outcome = download_artifact(
            artifact_download_url, TMP_DIRECTORY, "binaries", TOKEN)
        if curl_command_outcome.returncode != 0:
            exit(1)

        log("Unzipping binaries.zip...")
        unzip_command_outcome = unzip(TMP_DIRECTORY, "binaries")
        if unzip_command_outcome.returncode != 0:
            exit(1)

        steps_done += 1
        log("Done binaries!")

if steps_done != 2:
    print("Bad binaries/wasm!")
    exit(1)

template = download_genesis_template(REPOSITORY_OWNER, template_name, TMP_DIRECTORY)
template_command_outcome = generate_genesis_template(TMP_DIRECTORY, 'namada-{}'.format(short_sha), TMP_DIRECTORY)

genesis_folder_path = template_command_outcome.stdout.splitlines()[-2].split(" ")[4]
release_archive_path = template_command_outcome.stdout.splitlines()[-1].split(" ")[4]

log("Genesis folder: {}".format(genesis_folder_path))
log("Archive: {}".format(release_archive_path))





