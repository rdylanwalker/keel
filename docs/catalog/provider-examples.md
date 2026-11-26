# Provider Blueprint Examples

Each example below provides a ready-to-use `catalog.yaml` (`CatalogInfraBlueprint`) and `Pulumi.yaml` pair that you can place under `catalog/infra/<blueprint-id>/` in your GitOps repository and merge via PR.

Remember to enable the corresponding provider on **Control Plane & Providers** (or in `providers/registry.yaml`) so the blueprint appears in **Catalog & Form Wizard**.

> **Credential & API Key Secrets:** Properties marked with `writeOnly: true` (and `format: password`) are partitioned out of plaintext `Stack.spec.config` and encrypted with SOPS + `age` into `secret.enc.yaml`. A property named like an environment variable (`HCLOUD_TOKEN`) is injected into the PKO v2 workspace pod via `Stack.spec.envRefs` under that exact name; any other secret property (`dbPassword`) becomes Pulumi secret config of the same name via `Stack.spec.secretsRef`. See [Catalog overview](overview.md) for the AWS/Azure/GCP credential mappings.

---

## Multi-Cloud Credential Acquisition & Setup

Before deploying cloud blueprints or running live cloud tests, configure credentials for your cloud provider:

### 1. AWS Credential Acquisition (Console / CLI)
- Create a least-privilege IAM user or role for testing EC2 and S3:
  ```bash
  aws iam create-user --user-name keel-operator
  aws iam attach-user-policy --user-name keel-operator --policy-arn arn:aws:iam::aws:policy/AmazonEC2FullAccess
  aws iam attach-user-policy --user-name keel-operator --policy-arn arn:aws:iam::aws:policy/AmazonS3FullAccess
  aws iam create-access-key --user-name keel-operator
  ```
- Configure in your shell or catalog form:
  - `AWS_ACCESS_KEY_ID`: `AKIA...`
  - `AWS_SECRET_ACCESS_KEY`: `wJalr...`
  - `AWS_REGION`: `us-east-1` (Free Tier eligible)
  - Toggle for E2E tests: `export KEEL_E2E_CLOUD_AWS=1`

### 2. Azure Credential Acquisition (Portal / CLI)
- Create an Azure Service Principal scoped to your test subscription:
  ```bash
  az login
  SUBSCRIPTION_ID=$(az account show --query id -o tsv)
  az ad sp create-for-rbac --name "keel-operator-sp" \
    --role "Contributor" \
    --scopes "/subscriptions/${SUBSCRIPTION_ID}" \
    --query '{client_id: appId, client_secret: password, tenant_id: tenant}'
  ```
- Configure in your shell or catalog form:
  - `ARM_CLIENT_ID`: `<appId>`
  - `ARM_CLIENT_SECRET`: `<password>`
  - `ARM_TENANT_ID`: `<tenant>`
  - `ARM_SUBSCRIPTION_ID`: `<subscriptionId>`
  - `AZURE_LOCATION`: `eastus` (Free Tier eligible)
  - Toggle for E2E tests: `export KEEL_E2E_CLOUD_AZURE=1`

### 3. GCP Credential Acquisition (Console / CLI)
- In Google Cloud Console or `gcloud`, enable `compute.googleapis.com` and create a service account with `Compute Admin` and `Storage Admin` roles:
  ```bash
  PROJECT_ID=$(gcloud config get-value project)
  gcloud services enable compute.googleapis.com
  gcloud iam service-accounts create keel-operator \
    --description="Keel Operator Service Account" \
    --display-name="keel-operator"
  gcloud projects add-iam-policy-binding ${PROJECT_ID} \
    --member="serviceAccount:keel-operator@${PROJECT_ID}.iam.gserviceaccount.com" \
    --role="roles/compute.admin"
  gcloud projects add-iam-policy-binding ${PROJECT_ID} \
    --member="serviceAccount:keel-operator@${PROJECT_ID}.iam.gserviceaccount.com" \
    --role="roles/storage.admin"
  gcloud iam service-accounts keys create ./gcp-key.json \
    --iam-account="keel-operator@${PROJECT_ID}.iam.gserviceaccount.com"
  ```
- Configure in your shell or catalog form:
  - `GOOGLE_CREDENTIALS`: contents of `gcp-key.json` (or path via `GOOGLE_APPLICATION_CREDENTIALS`)
  - `GCP_PROJECT`: `${PROJECT_ID}`
  - `GCP_ZONE`: `us-central1-a` (Always Free tier eligible)
  - Toggle for E2E tests: `export KEEL_E2E_CLOUD_GCP=1`

---

## Local `kind` Workload Cluster (`kind`)

Place in `catalog/infra/kind-cluster/`.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: kind-cluster
  name: Local kind Workload Cluster
  description: Provision a local kind workload cluster via tehcyx/kind.
  category: kubernetes-cluster
  provider: kind
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - clusterName
    - endpoint
    - kubeconfig
  schema:
    type: object
    required: [clusterName, kubernetesVersion, workerCount]
    properties:
      clusterName:
        type: string
        title: Cluster Name
        default: workload-dev
        pattern: "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$"
      kubernetesVersion:
        type: string
        title: Kubernetes Version
        default: v1.35.8
        enum: [v1.37.0, v1.36.4, v1.35.8, v1.34.11, v1.33.12]
      workerCount:
        type: integer
        title: Worker Node Count
        default: 1
        minimum: 0
        maximum: 3
```

### `Pulumi.yaml`

```yaml
name: kind-cluster
runtime: yaml
description: Local kind workload cluster provisioned via tehcyx/kind
packages:
  kind:
    source: terraform-provider
    parameters:
      - tehcyx/kind
      - 0.13.0
config:
  clusterName:
    type: string
    default: workload-dev
  kubernetesVersion:
    type: string
    default: v1.35.8
  workerCount:
    type: integer
    default: 1
  kubeconfigSecretName:
    type: string
    default: workload-dev-kubeconfig
  kubeconfigSecretNamespace:
    type: string
    default: tenant-default
variables:
  # kubelet pulls images one at a time by default; pull in parallel on every node.
  kubeletParallelImagePulls:
    - |
      kind: KubeletConfiguration
      serializeImagePulls: false
      maxParallelImagePulls: 5
  nodeTopologies:
    - - role: control-plane
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
    - - role: control-plane
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
      - role: worker
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
    - - role: control-plane
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
      - role: worker
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
      - role: worker
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
    - - role: control-plane
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
      - role: worker
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
      - role: worker
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
      - role: worker
        kubeadmConfigPatches: ${kubeletParallelImagePulls}
resources:
  workloadCluster:
    type: kind:index:Cluster
    properties:
      name: ${clusterName}
      nodeImage: kindest/node:${kubernetesVersion}
      kubeconfigPath: /tmp/${clusterName}-kubeconfig
      waitForReady: false
      kindConfig:
        apiVersion: kind.x-k8s.io/v1alpha4
        kind: Cluster
        nodes:
          fn::select:
            - ${workerCount}
            - ${nodeTopologies}
  kubeconfigSecret:
    type: kubernetes:core/v1:Secret
    properties:
      metadata:
        name: ${kubeconfigSecretName}
        namespace: ${kubeconfigSecretNamespace}
      stringData:
        kubeconfig:
          fn::secret:
            fn::join:
              - https://${clusterName}-control-plane:6443
              - fn::split:
                  - ${workloadCluster.endpoint}
                  - ${workloadCluster.kubeconfig}
outputs:
  clusterName: ${workloadCluster.name}
  endpoint: ${workloadCluster.endpoint}
  kubeconfig:
    fn::secret: ${workloadCluster.kubeconfig}
```

---

## AWS EKS Cluster (`aws`)

Place in `catalog/infra/aws-eks/`.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: aws-eks
  name: AWS EKS Workload Cluster
  description: Provision an Amazon EKS cluster with a managed node group.
  category: kubernetes-cluster
  provider: aws
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - clusterEndpoint
  schema:
    type: object
    required:
      - clusterName
      - region
      - instanceType
      - desiredCapacity
      - AWS_ACCESS_KEY_ID
      - AWS_SECRET_ACCESS_KEY
    properties:
      clusterName:
        type: string
        title: Cluster Name
        default: eks-workload
      region:
        type: string
        title: AWS Region
        default: us-east-1
        enum: [us-east-1, us-west-2, eu-west-1, eu-central-1]
      instanceType:
        type: string
        title: Node Instance Type
        default: t3.medium
      desiredCapacity:
        type: integer
        title: Desired Node Count
        default: 2
        minimum: 1
        maximum: 10
      AWS_ACCESS_KEY_ID:
        type: string
        title: AWS Access Key ID
        format: password
        writeOnly: true
      AWS_SECRET_ACCESS_KEY:
        type: string
        title: AWS Secret Access Key
        format: password
        writeOnly: true
  uiSchema:
    AWS_ACCESS_KEY_ID:
      ui:widget: password
      ui:help: "Encrypted via SOPS + age and bound to Stack.spec.envRefs."
    AWS_SECRET_ACCESS_KEY:
      ui:widget: password
      ui:help: "Encrypted via SOPS + age and bound to Stack.spec.envRefs."
```

### `Pulumi.yaml`

```yaml
name: aws-eks
runtime: yaml
description: Managed Amazon EKS cluster
config:
  clusterName:
    type: string
  region:
    type: string
    default: us-east-1
  instanceType:
    type: string
    default: t3.medium
  desiredCapacity:
    type: integer
    default: 2
resources:
  awsProvider:
    type: pulumi:providers:aws
    properties:
      region: ${region}
  vpc:
    type: awsx:ec2:Vpc
    options:
      provider: ${awsProvider}
    properties:
      cidrBlock: 10.20.0.0/16
  cluster:
    type: eks:Cluster
    options:
      provider: ${awsProvider}
    properties:
      name: ${clusterName}
      vpcId: ${vpc.vpcId}
      subnetIds: ${vpc.privateSubnetIds}
      instanceType: ${instanceType}
      desiredCapacity: ${desiredCapacity}
outputs:
  clusterEndpoint: ${cluster.eksCluster.endpoint}
```

---

## AWS EC2 Virtual Machine (`aws-vm`)

Place in `catalog/infra/aws-vm/`. Provisions an EC2 virtual machine (defaulting to free-tier eligible `t3.micro`) with security group, SSH key pair, and direct SSH outputs.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: aws-vm
  name: AWS EC2 Virtual Machine
  description: Provision an AWS EC2 virtual machine with SSH access (defaults to free-tier t3.micro)
  category: compute
  provider: aws
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: true
  outputs:
    - instanceId
    - publicIp
    - sshHost
    - sshUser
    - sshPort
  schema:
    type: object
    required:
      - sshPublicKey
    properties:
      region:
        type: string
        title: AWS Region
        default: us-east-1
      instanceType:
        type: string
        title: EC2 Instance Type
        default: t3.micro
        enum:
          - t3.micro
          - t2.micro
      sshUser:
        type: string
        title: SSH Username
        default: ubuntu
      sshPublicKey:
        type: string
        title: Operator SSH Public Key
      awsAccessKeyId:
        type: string
        title: AWS Access Key ID
        writeOnly: true
      awsSecretAccessKey:
        type: string
        title: AWS Secret Access Key
        writeOnly: true
        format: password
  uiSchema:
    sshPublicKey:
      ui:widget: textarea
      ui:help: "SSH public key injected into the EC2 instance key pair."
    awsSecretAccessKey:
      ui:widget: password
      ui:help: "Encrypted via SOPS + tenant age key before committing to Git."
```

### `Pulumi.yaml`

```yaml
name: aws-vm
runtime: yaml
description: Free-tier eligible AWS EC2 virtual machine

config:
  region:
    type: string
    default: us-east-1
  instanceType:
    type: string
    default: t3.micro
  sshUser:
    type: string
    default: ubuntu
  sshPublicKey:
    type: string
  amiId:
    type: string
    default: ""
  instanceName:
    type: string
    default: keel-free-vm

variables:
  ubuntuAmi:
    fn::invoke:
      function: aws:ec2:getAmi
      arguments:
        mostRecent: true
        owners:
          - "099720109477"
        filters:
          - name: name
            values:
              - "ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"
          - name: virtualization-type
            values:
              - hvm

resources:
  sshKeyPair:
    type: aws:ec2:KeyPair
    properties:
      publicKey: ${sshPublicKey}
      tags:
        managed-by: keel

  vmSecurityGroup:
    type: aws:ec2:SecurityGroup
    properties:
      description: Security group allowing inbound SSH traffic
      ingress:
        - protocol: tcp
          fromPort: 22
          toPort: 22
          cidrBlocks:
            - 0.0.0.0/0
      egress:
        - protocol: "-1"
          fromPort: 0
          toPort: 0
          cidrBlocks:
            - 0.0.0.0/0
      tags:
        managed-by: keel

  ec2Instance:
    type: aws:ec2:Instance
    properties:
      instanceType: ${instanceType}
      ami: ${amiId != "" ? amiId : ubuntuAmi.id}
      keyName: ${sshKeyPair.keyName}
      vpcSecurityGroupIds:
        - ${vmSecurityGroup.id}
      rootBlockDevice:
        volumeSize: 20
        volumeType: gp3
        encrypted: true
      tags:
        Name: ${instanceName}
        managed-by: keel

outputs:
  instanceId: ${ec2Instance.id}
  publicIp: ${ec2Instance.publicIp}
  sshHost: ${ec2Instance.publicIp}
  sshUser: ${sshUser}
  sshPort: 22
```

---

## Google Cloud GKE Cluster (`gcp`)

Place in `catalog/infra/gcp-gke/`.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: gcp-gke
  name: GCP GKE Autopilot Cluster
  description: Provision a managed GKE Autopilot cluster on Google Cloud.
  category: kubernetes-cluster
  provider: gcp
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - endpoint
  schema:
    type: object
    required: [clusterName, project, location, GOOGLE_CREDENTIALS]
    properties:
      clusterName:
        type: string
        title: Cluster Name
        default: gke-workload
        pattern: "^[a-z]([a-z0-9-]{0,38}[a-z0-9])?$"
      project:
        type: string
        title: GCP Project ID
      location:
        type: string
        title: GCP Region
        default: us-central1
        enum: [us-central1, us-east1, europe-west1]
      GOOGLE_CREDENTIALS:
        type: string
        title: GCP Service Account JSON Key
        format: password
        writeOnly: true
  uiSchema:
    GOOGLE_CREDENTIALS:
      ui:widget: password
      ui:help: "Service account JSON key encrypted via SOPS + age and bound to Stack.spec.envRefs."
```

### `Pulumi.yaml`

```yaml
name: gcp-gke
runtime: yaml
description: Google Kubernetes Engine Autopilot cluster
config:
  clusterName:
    type: string
  project:
    type: string
  location:
    type: string
    default: us-central1
resources:
  gke:
    type: gcp:container:Cluster
    properties:
      name: ${clusterName}
      project: ${project}
      location: ${location}
      enableAutopilot: true
outputs:
  endpoint: ${gke.endpoint}
```

---

## GCP Compute Engine VM (`gcp-vm`)

Place in `catalog/infra/gcp-vm/`. Provisions a Compute Engine VM on Google Cloud (defaulting to Always-Free eligible `e2-micro`) with SSH firewall rule, metadata SSH key, and direct SSH outputs.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: gcp-vm
  name: GCP Compute Engine VM
  description: Provision a GCP Compute Engine virtual machine with SSH access (defaults to Always-Free e2-micro)
  category: compute
  provider: gcp
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: true
  outputs:
    - instanceName
    - publicIp
    - sshHost
    - sshUser
    - sshPort
  schema:
    type: object
    required:
      - project
      - sshPublicKey
    properties:
      project:
        type: string
        title: GCP Project ID
      region:
        type: string
        title: GCP Region
        default: us-central1
      zone:
        type: string
        title: GCP Zone
        default: us-central1-a
      machineType:
        type: string
        title: Machine Type
        default: e2-micro
      sshUser:
        type: string
        title: SSH Username
        default: debian
      sshPublicKey:
        type: string
        title: Operator SSH Public Key
      googleCredentials:
        type: string
        title: GCP Service Account JSON Key
        writeOnly: true
        format: password
  uiSchema:
    sshPublicKey:
      ui:widget: textarea
      ui:help: "SSH public key injected into GCP instance metadata."
    googleCredentials:
      ui:widget: password
      ui:help: "Encrypted via SOPS + tenant age key before committing to Git."
```

### `Pulumi.yaml`

```yaml
name: gcp-vm
runtime: yaml
description: Always-Free eligible GCP e2-micro compute instance

config:
  project:
    type: string
  region:
    type: string
    default: us-central1
  zone:
    type: string
    default: us-central1-a
  machineType:
    type: string
    default: e2-micro
  sshUser:
    type: string
    default: debian
  sshPublicKey:
    type: string
  instanceName:
    type: string
    default: keel-free-vm

resources:
  sshFirewall:
    type: gcp:compute:Firewall
    properties:
      project: ${project}
      network: default
      description: Allow inbound SSH from any IP
      allows:
        - protocol: tcp
          ports:
            - "22"
      sourceRanges:
        - 0.0.0.0/0
      targetTags:
        - allow-ssh

  computeInstance:
    type: gcp:compute:Instance
    properties:
      project: ${project}
      name: ${instanceName}
      machineType: ${machineType}
      zone: ${zone}
      tags:
        - allow-ssh
      bootDisk:
        initializeParams:
          image: debian-cloud/debian-12
          size: 10
          type: pd-standard
      networkInterfaces:
        - network: default
          accessConfigs:
            - {}
      metadata:
        ssh-keys: "${sshUser}:${sshPublicKey}"
      labels:
        managed-by: keel

outputs:
  instanceName: ${computeInstance.name}
  publicIp: ${computeInstance.networkInterfaces[0].accessConfigs[0].natIp}
  sshHost: ${computeInstance.networkInterfaces[0].accessConfigs[0].natIp}
  sshUser: ${sshUser}
  sshPort: 22
```

---

## Microsoft Azure AKS Cluster (`azure`)

Place in `catalog/infra/azure-aks/`.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: azure-aks
  name: Azure AKS Workload Cluster
  description: Provision a managed AKS cluster in a dedicated Resource Group.
  category: kubernetes-cluster
  provider: azure
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - fqdn
  schema:
    type: object
    required:
      - clusterName
      - location
      - nodeCount
      - vmSize
      - ARM_CLIENT_ID
      - ARM_CLIENT_SECRET
      - ARM_TENANT_ID
      - ARM_SUBSCRIPTION_ID
    properties:
      clusterName:
        type: string
        title: Cluster Name
        default: aks-workload
      location:
        type: string
        title: Azure Region
        default: eastus
      nodeCount:
        type: integer
        title: Node Count
        default: 2
        minimum: 1
        maximum: 10
      vmSize:
        type: string
        title: Node VM Size
        default: Standard_D2s_v3
      ARM_CLIENT_ID:
        type: string
        title: Azure Service Principal Client ID
        format: password
        writeOnly: true
      ARM_CLIENT_SECRET:
        type: string
        title: Azure Service Principal Client Secret
        format: password
        writeOnly: true
      ARM_TENANT_ID:
        type: string
        title: Azure Tenant ID
        format: password
        writeOnly: true
      ARM_SUBSCRIPTION_ID:
        type: string
        title: Azure Subscription ID
        format: password
        writeOnly: true
  uiSchema:
    ARM_CLIENT_ID:
      ui:widget: password
    ARM_CLIENT_SECRET:
      ui:widget: password
    ARM_TENANT_ID:
      ui:widget: password
    ARM_SUBSCRIPTION_ID:
      ui:widget: password
```

### `Pulumi.yaml`

```yaml
name: azure-aks
runtime: yaml
description: Azure Kubernetes Service cluster
config:
  clusterName:
    type: string
  location:
    type: string
    default: eastus
  nodeCount:
    type: integer
    default: 2
  vmSize:
    type: string
    default: Standard_D2s_v3
resources:
  rg:
    type: azure-native:resources:ResourceGroup
    properties:
      resourceGroupName: rg-${clusterName}
      location: ${location}
  aks:
    type: azure-native:containerservice:ManagedCluster
    properties:
      resourceGroupName: ${rg.name}
      resourceName: ${clusterName}
      location: ${location}
      dnsPrefix: ${clusterName}
      identity:
        type: SystemAssigned
      agentPoolProfiles:
        - name: system
          count: ${nodeCount}
          vmSize: ${vmSize}
          mode: System
outputs:
  fqdn: ${aks.fqdn}
```

---

## Azure Linux Virtual Machine (`azure-vm`)

Place in `catalog/infra/azure-vm/`. Provisions a Linux VM in Microsoft Azure (defaulting to free-tier eligible `Standard_B1s`) with Resource Group, VNet, Subnet, NSG, Public IP, and direct SSH outputs.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: azure-vm
  name: Azure Linux Virtual Machine
  description: Provision an Azure Linux virtual machine with SSH access (defaults to free-tier Standard_B1s)
  category: compute
  provider: azure
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: true
  outputs:
    - vmId
    - publicIp
    - sshHost
    - sshUser
    - sshPort
  schema:
    type: object
    required:
      - sshPublicKey
    properties:
      location:
        type: string
        title: Azure Region
        default: eastus
      vmSize:
        type: string
        title: Virtual Machine Size
        default: Standard_B1s
      sshUser:
        type: string
        title: SSH Username
        default: azureuser
      sshPublicKey:
        type: string
        title: Operator SSH Public Key
      armClientId:
        type: string
        title: Azure Client ID
        writeOnly: true
      armClientSecret:
        type: string
        title: Azure Client Secret
        writeOnly: true
        format: password
      armTenantId:
        type: string
        title: Azure Tenant ID
        writeOnly: true
      armSubscriptionId:
        type: string
        title: Azure Subscription ID
        writeOnly: true
  uiSchema:
    sshPublicKey:
      ui:widget: textarea
      ui:help: "SSH public key injected for the administrative user."
    armClientSecret:
      ui:widget: password
      ui:help: "Encrypted via SOPS + tenant age key before committing to Git."
```

### `Pulumi.yaml`

```yaml
name: azure-vm
runtime: yaml
description: Free-tier eligible Azure Standard_B1s Linux virtual machine

config:
  location:
    type: string
    default: eastus
  vmSize:
    type: string
    default: Standard_B1s
  sshUser:
    type: string
    default: azureuser
  sshPublicKey:
    type: string
  vmName:
    type: string
    default: keel-free-vm

resources:
  resourceGroup:
    type: azure-native:resources:ResourceGroup
    properties:
      location: ${location}
      tags:
        managed-by: keel

  virtualNetwork:
    type: azure-native:network:VirtualNetwork
    properties:
      resourceGroupName: ${resourceGroup.name}
      location: ${location}
      addressSpace:
        addressPrefixes:
          - 10.0.0.0/16
      tags:
        managed-by: keel

  subnet:
    type: azure-native:network:Subnet
    properties:
      resourceGroupName: ${resourceGroup.name}
      virtualNetworkName: ${virtualNetwork.name}
      addressPrefix: 10.0.1.0/24

  publicIp:
    type: azure-native:network:PublicIPAddress
    properties:
      resourceGroupName: ${resourceGroup.name}
      location: ${location}
      publicIPAllocationMethod: Static
      sku:
        name: Standard
      tags:
        managed-by: keel

  networkSecurityGroup:
    type: azure-native:network:NetworkSecurityGroup
    properties:
      resourceGroupName: ${resourceGroup.name}
      location: ${location}
      securityRules:
        - name: AllowSSH
          protocol: Tcp
          direction: Inbound
          priority: 1000
          access: Allow
          sourceAddressPrefix: "*"
          sourcePortRange: "*"
          destinationAddressPrefix: "*"
          destinationPortRange: "22"
      tags:
        managed-by: keel

  networkInterface:
    type: azure-native:network:NetworkInterface
    properties:
      resourceGroupName: ${resourceGroup.name}
      location: ${location}
      networkSecurityGroup:
        id: ${networkSecurityGroup.id}
      ipConfigurations:
        - name: ipconfig1
          subnet:
            id: ${subnet.id}
          publicIPAddress:
            id: ${publicIp.id}
          privateIPAllocationMethod: Dynamic
      tags:
        managed-by: keel

  virtualMachine:
    type: azure-native:compute:VirtualMachine
    properties:
      resourceGroupName: ${resourceGroup.name}
      location: ${location}
      vmName: ${vmName}
      hardwareProfile:
        vmSize: ${vmSize}
      networkProfile:
        networkInterfaces:
          - id: ${networkInterface.id}
            primary: true
      storageProfile:
        imageReference:
          publisher: Canonical
          offer: 0001-com-ubuntu-server-jammy
          sku: 22_04-lts-gen2
          version: latest
        osDisk:
          createOption: FromImage
          diskSizeGB: 30
          managedDisk:
            storageAccountType: Standard_LRS
      osProfile:
        computerName: ${vmName}
        adminUsername: ${sshUser}
        linuxConfiguration:
          disablePasswordAuthentication: true
          ssh:
            publicKeys:
              - keyData: ${sshPublicKey}
                path: /home/${sshUser}/.ssh/authorized_keys
      tags:
        managed-by: keel

outputs:
  vmId: ${virtualMachine.id}
  publicIp: ${publicIp.ipAddress}
  sshHost: ${publicIp.ipAddress}
  sshUser: ${sshUser}
  sshPort: 22
```

---

## Proxmox VE k3s Cluster (`proxmox`)

Seeded as `catalog/infra/proxmox-k3s-cluster/`. It clones one VM from a cloud-init template, installs single-node `k3s` (`--disable=traefik`) over SSH and writes the admin kubeconfig, with its `127.0.0.1` server rewritten to the VM address, to the tenant's kubeconfig Secret. The result is a workload cluster like `kind-cluster`: pick it as the target cluster of an app. It works like the [Stage 0 Proxmox target](../getting-started/bootstrap.md#proxmox-ve---target-proxmox), but runs in the tenant's PKO workspace pod. It is not covered by automated tests.

- **Template:** the same as for Stage 0: a cloud-init image with `qemu-guest-agent` installed and enabled, boot disk on `scsi0` (`diskInterface`), default VM ID `9000` (`templateVmId`). Proxmox VE picks the new VM's ID. Give the VM a DHCP reservation: the kubeconfig and the k3s certificate (`--tls-san`) use the address the guest agent reports for the first NIC.
- **Credentials:** `PROXMOX_VE_ENDPOINT` and `PROXMOX_VE_API_TOKEN` are secret fields, injected into the workspace pod as environment variables of the same name. The token needs the privileges listed for [Stage 0](../getting-started/bootstrap.md#proxmox-ve---target-proxmox), including guest-agent read access. Turn on *Skip Proxmox VE TLS Verification* (`proxmoxInsecure`) for a self-signed API certificate.
- **SSH:** the program generates an ED25519 key pair (`tls:index:PrivateKey`) that only lives in the encrypted Stack state; its public key and your *Operator SSH Public Key* are added for `sshUser` (default `keel`) through the VM's cloud-init drive. `k3s` is installed once: later edits to the install script or a new DHCP lease do not re-run it.
- **Kubernetes version:** `kubernetesVersion` is a k3s release channel (`v1.35` installs the latest `v1.35.x+k3s` release). It applies only when the cluster is created: the install step ignores later changes, so editing it on an existing instance shows no diff in the preview, merges cleanly and leaves k3s at its original version. Upgrade k3s on the node yourself (for example with the [k3s system-upgrade-controller](https://docs.k3s.io/upgrades/automated)), or create a new instance.
- **Kubeconfig:** the `kubeconfig` output and the `<instance>-kubeconfig` Secret hold the k3s admin kubeconfig (a cluster-admin client certificate). The fetch step logs nothing, and its output is a Pulumi secret. The program also binds the tenant's `tenant-<id>-sa` identity to `cluster-admin` on the new cluster, as `kind-cluster` does, so Flux can impersonate it there.
- **Network:** the PKO workspace pod must reach the Proxmox VE API and the VM on TCP `22` and `6443`; its NetworkPolicy allows private addresses on 22, 443, 6443 and 8006 only. Flux on the management cluster must reach the VM on `6443`. The program creates no host firewall, so SSH and the k3s API (whose admin kubeconfig is cluster-admin) are open to everything that can reach the VM's bridge or VLAN. That is wider than the cloud VM programs, whose security groups limit 22 and 6443 to `allowedManagementCidrs`. Put the VM on an isolated VLAN or add PVE firewall rules that admit 22 and 6443 only from the PKO workspace and Flux source addresses.

### `catalog.yaml`

```yaml
# Proxmox VE single-node k3s workload cluster blueprint (`SDD.md` §4.1, `NFR-EXT-02`). It clones
# one VM from a cloud-init template, installs k3s over SSH with a generated key (like the Stage 0
# Proxmox program) and writes the admin kubeconfig to the tenant's kubeconfig Secret, so the
# result is a cluster apps can target, like `kind-cluster` and the managed-cluster blueprints.
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: proxmox-k3s-cluster
  name: Proxmox VE k3s Cluster (single node)
  description: Clone one VM from a cloud-init template on Proxmox VE via the bpg/proxmox OpenTofu/Terraform bridge and install single-node k3s on it as a workload cluster.
  category: kubernetes-cluster
  provider: proxmox
  resourceKind: cluster
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: true
  outputs:
    - clusterName
    - endpoint
    - kubeconfig
    - controlPlaneIp
    - vmId
    - sshHost
    - sshUser
    - sshPort
  schema:
    type: object
    required:
      - clusterName
      - nodeName
      - templateVmId
      - kubernetesVersion
      - sshPublicKey
    properties:
      clusterName:
        type: string
        title: Cluster Name (VM name prefix)
        pattern: "^[a-z0-9]([a-z0-9-]{0,55}[a-z0-9])?$"
      nodeName:
        type: string
        title: Proxmox Target Node
        default: pve1
      templateVmId:
        type: integer
        title: Cloud-Init Template VM ID
        description: Template with cloud-init and qemu-guest-agent; its boot disk must be on the Boot Disk Interface.
        default: 9000
        minimum: 100
      datastoreId:
        type: string
        title: Datastore
        description: Datastore for the cloned disk and the cloud-init drive.
        default: local-lvm
      networkBridge:
        type: string
        title: Network Bridge
        default: vmbr0
      cores:
        type: integer
        title: vCPU Cores
        default: 4
        minimum: 2
        maximum: 32
      memoryMb:
        type: integer
        title: Memory (MiB)
        default: 8192
        minimum: 2048
      diskSizeGb:
        type: integer
        title: Boot Disk Size (GiB)
        description: Must be at least the template's disk size.
        default: 32
        minimum: 10
      diskInterface:
        type: string
        title: Boot Disk Interface
        description: The template's boot-disk interface (bpg/proxmox matches cloned disks by interface).
        default: scsi0
      kubernetesVersion:
        type: string
        title: Kubernetes Version (k3s channel)
        description: k3s release channel; the latest k3s patch release of that Kubernetes minor is installed. Applies only when the cluster is created; changing it later does not upgrade k3s.
        default: v1.35
        enum:
          - v1.36
          - v1.35
          - v1.34
          - v1.33
      sshUser:
        type: string
        title: SSH Username
        default: keel
      sshPublicKey:
        type: string
        title: Operator SSH Public Key
      proxmoxInsecure:
        type: boolean
        title: Skip Proxmox VE TLS Verification
        description: Accept a self-signed Proxmox VE API certificate.
        default: false
      PROXMOX_VE_ENDPOINT:
        type: string
        title: Proxmox VE API Endpoint
        writeOnly: true
      PROXMOX_VE_API_TOKEN:
        type: string
        title: Proxmox VE API Token
        writeOnly: true
        format: password
  uiSchema:
    sshPublicKey:
      ui:widget: textarea
      ui:help: "Your ed25519 or RSA public key, added next to the key Pulumi generates to install k3s."
    PROXMOX_VE_ENDPOINT:
      ui:placeholder: "https://pve.example.local:8006/"
      ui:help: "Injected as PROXMOX_VE_ENDPOINT via Stack.spec.envRefs. The PKO workspace must reach it."
    PROXMOX_VE_API_TOKEN:
      ui:widget: password
      ui:help: "Format: user@realm!tokenid=uuid. Encrypted via SOPS + tenant age key before committing to Git."
```

### `Pulumi.yaml`

```yaml
# Proxmox VE single-node k3s workload cluster (`SDD.md` §4.1, `NFR-EXT-02`) via the bpg/proxmox
# OpenTofu/Terraform bridge. The VM is cloned from a cloud-init template (with
# qemu-guest-agent) whose built-in cloud-init drive injects a generated SSH key plus the
# operator's key; k3s is then installed over SSH from the PKO workspace (no `snippets` datastore
# or PVE-node SSH access is needed), like the Stage 0 Proxmox program. The admin kubeconfig is
# rewritten to the VM address, written to the tenant's kubeconfig Secret for Flux and exported
# as a secret output.
#
# Provider credentials come from the workspace environment: the blueprint's secret fields
# `PROXMOX_VE_ENDPOINT` and `PROXMOX_VE_API_TOKEN` become `Stack.spec.envRefs` of the same name.
name: proxmox-k3s-cluster
runtime: yaml
description: Single-node k3s workload cluster on a Proxmox VE VM (bpg/proxmox)

packages:
  proxmox:
    source: terraform-provider
    version: 1.5.1
    parameters:
      - bpg/proxmox
      - 0.71.0

config:
  clusterName:
    type: string
  nodeName:
    type: string
    default: pve1
  templateVmId:
    type: integer
    default: 9000
  datastoreId:
    type: string
    default: local-lvm
  networkBridge:
    type: string
    default: vmbr0
  cores:
    type: integer
    default: 4
  memoryMb:
    type: integer
    default: 8192
  diskSizeGb:
    type: integer
    default: 32
  # Must be the template's boot-disk interface: bpg/proxmox matches clone disks by interface,
  # so any other value adds a blank disk and leaves the boot disk at the image size.
  diskInterface:
    type: string
    default: scsi0
  sshUser:
    type: string
    default: keel
  sshPublicKey:
    type: string
  # k3s release channel (one per Kubernetes minor, https://update.k3s.io/v1-release/channels).
  # Used only by the one-shot install (installK3s ignores `create` changes): changing it later
  # does not upgrade k3s.
  kubernetesVersion:
    type: string
    default: v1.35
  proxmoxInsecure:
    type: boolean
    default: false
  kubeconfigSecretName:
    type: string
    default: proxmox-k3s-cluster-kubeconfig
  kubeconfigSecretNamespace:
    type: string
    default: tenant-default

variables:
  # Guest-agent addresses per interface; index 0 is the loopback, index 1 the first NIC.
  controlPlaneIp: ${k3sControlPlaneVm.ipv4Addresses[1][0]}
  # k3s admin kubeconfig with the API server rewritten from 127.0.0.1 to the VM address.
  workloadKubeconfig:
    fn::secret: ${fetchKubeconfig.stdout}

resources:
  # Endpoint and API token are read from PROXMOX_VE_ENDPOINT / PROXMOX_VE_API_TOKEN.
  pveProvider:
    type: pulumi:providers:proxmox
    properties:
      insecure: ${proxmoxInsecure}

  # Key Pulumi uses to install k3s and read the kubeconfig. The private half only ever lives in
  # the (encrypted) Stack state.
  controlPlaneSshKey:
    type: tls:index:PrivateKey
    properties:
      algorithm: ED25519

  k3sControlPlaneVm:
    type: proxmox:index/virtualEnvironmentVm:VirtualEnvironmentVm
    options:
      provider: ${pveProvider}
    properties:
      name: ${clusterName}-cp-01
      nodeName: ${nodeName}
      description: Keel k3s workload cluster ${clusterName} (single node)
      tags:
        - keel-workload
        - k3s
      onBoot: true
      agent:
        enabled: true
      clone:
        vmId: ${templateVmId}
        full: true
      cpu:
        cores: ${cores}
        type: x86-64-v2-AES
      memory:
        dedicated: ${memoryMb}
      # PVE's default for new VMs; lets `iothread` apply to SCSI disks.
      scsiHardware: virtio-scsi-single
      disks:
        - datastoreId: ${datastoreId}
          interface: ${diskInterface}
          size: ${diskSizeGb}
          iothread: true
          discard: "on"
      networkDevices:
        - bridge: ${networkBridge}
          model: virtio
      initialization:
        datastoreId: ${datastoreId}
        ipConfigs:
          - ipv4:
              address: dhcp
        userAccount:
          username: ${sshUser}
          keys:
            - ${controlPlaneSshKey.publicKeyOpenssh}
            - ${sshPublicKey}

  installK3s:
    type: command:remote:Command
    options:
      # The install runs once. Editing the script or a new DHCP lease must not re-run it
      # against an existing node on the next resync.
      ignoreChanges:
        - create
        - connection
    properties:
      connection:
        host: ${controlPlaneIp}
        user: ${sshUser}
        privateKey: ${controlPlaneSshKey.privateKeyOpenssh}
      create: |
        set -eu
        cloud-init status --wait >/dev/null 2>&1 || true
        if [ ! -f /etc/rancher/k3s/k3s.yaml ]; then
          curl -sfL https://get.k3s.io | sudo INSTALL_K3S_CHANNEL="${kubernetesVersion}" INSTALL_K3S_EXEC="server --disable=traefik --tls-san=${controlPlaneIp} --write-kubeconfig-mode=644" sh -
        fi

  fetchKubeconfig:
    type: command:remote:Command
    options:
      dependsOn:
        - ${installK3s}
      # stdout is the k3s admin kubeconfig: keep it encrypted in state and out of the logs.
      additionalSecretOutputs:
        - stdout
    properties:
      logging: none
      connection:
        host: ${controlPlaneIp}
        user: ${sshUser}
        privateKey: ${controlPlaneSshKey.privateKeyOpenssh}
      create: |
        for i in $(seq 1 60); do
          if [ -s /etc/rancher/k3s/k3s.yaml ]; then
            sed "s/127.0.0.1/${controlPlaneIp}/g" /etc/rancher/k3s/k3s.yaml
            exit 0
          fi
          sleep 5
        done
        echo "Timed out waiting for /etc/rancher/k3s/k3s.yaml" >&2
        exit 1

  kubeconfigSecret:
    type: kubernetes:core/v1:Secret
    properties:
      metadata:
        name: ${kubeconfigSecretName}
        namespace: ${kubeconfigSecretNamespace}
      stringData:
        kubeconfig: ${workloadKubeconfig}

  # Kubernetes provider for the new workload cluster (the PKO workspace reaches the VM on 6443).
  workloadProvider:
    type: pulumi:providers:kubernetes
    properties:
      kubeconfig: ${workloadKubeconfig}

  # Flux reconciles tenant HelmReleases/Kustomizations on this cluster through the kubeconfig
  # Secret while impersonating the owning tenant's ServiceAccount
  # (`system:serviceaccount:tenant-<id>:tenant-<id>-sa`, Flux multi-tenancy lockdown). The tenant
  # owns this cluster outright (it holds its admin kubeconfig), so that identity is cluster-admin.
  tenantFluxAccess:
    type: kubernetes:rbac.authorization.k8s.io/v1:ClusterRoleBinding
    options:
      provider: ${workloadProvider}
    properties:
      metadata:
        name: keel-tenant-flux
      roleRef:
        apiGroup: rbac.authorization.k8s.io
        kind: ClusterRole
        name: cluster-admin
      subjects:
        - kind: ServiceAccount
          name: ${kubeconfigSecretNamespace}-sa
          namespace: ${kubeconfigSecretNamespace}

outputs:
  clusterName: ${clusterName}
  endpoint: https://${controlPlaneIp}:6443
  controlPlaneIp: ${controlPlaneIp}
  vmId: ${k3sControlPlaneVm.vmId}
  sshHost: ${controlPlaneIp}
  sshUser: ${sshUser}
  sshPort: 22
  kubeconfig:
    fn::secret: ${fetchKubeconfig.stdout}
```

---

## Hetzner Cloud Server (`hetzner`)

Place in `catalog/infra/hetzner-node/`.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: hetzner-node
  name: Hetzner Cloud Server
  description: Provision a Hetzner Cloud compute instance.
  category: compute
  provider: hetzner
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - ipv4Address
  schema:
    type: object
    required: [serverName, serverType, location, HCLOUD_TOKEN]
    properties:
      serverName:
        type: string
        title: Server Name
        default: hcloud-node-01
      serverType:
        type: string
        title: Server Type
        default: cx22
        enum: [cx22, cx32, cpx21, cpx31]
      location:
        type: string
        title: Datacenter Location
        default: fsn1
        enum: [fsn1, nbg1, hel1, ash]
      HCLOUD_TOKEN:
        type: string
        title: Hetzner Cloud API Token
        format: password
        writeOnly: true
  uiSchema:
    HCLOUD_TOKEN:
      ui:widget: password
      ui:help: "Encrypted via SOPS + age and bound to Stack.spec.envRefs."
```

### `Pulumi.yaml`

```yaml
name: hetzner-node
runtime: yaml
description: Hetzner Cloud server via pulumi/hcloud
config:
  serverName:
    type: string
  serverType:
    type: string
    default: cx22
  location:
    type: string
    default: fsn1
resources:
  server:
    type: hcloud:Server
    properties:
      name: ${serverName}
      serverType: ${serverType}
      image: ubuntu-24.04
      location: ${location}
outputs:
  ipv4Address: ${server.ipv4Address}
```

---

## Cloudflare DNS Record (`cloudflare`)

Place in `catalog/infra/cloudflare-dns/`.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: cloudflare-dns
  name: Cloudflare DNS Record
  description: Manage a proxied DNS record in Cloudflare.
  category: edge
  provider: cloudflare
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - hostname
  schema:
    type: object
    required: [zoneId, recordName, targetIp, CLOUDFLARE_API_TOKEN]
    properties:
      zoneId:
        type: string
        title: Cloudflare Zone ID
      recordName:
        type: string
        title: Subdomain / Hostname
        default: app
      targetIp:
        type: string
        title: Target IPv4 Address
      CLOUDFLARE_API_TOKEN:
        type: string
        title: Cloudflare API Token
        format: password
        writeOnly: true
  uiSchema:
    CLOUDFLARE_API_TOKEN:
      ui:widget: password
      ui:help: "Encrypted via SOPS + age and bound to Stack.spec.envRefs."
```

### `Pulumi.yaml`

```yaml
name: cloudflare-dns
runtime: yaml
description: Cloudflare DNS A record
config:
  zoneId:
    type: string
  recordName:
    type: string
  targetIp:
    type: string
resources:
  record:
    type: cloudflare:Record
    properties:
      zoneId: ${zoneId}
      name: ${recordName}
      type: A
      content: ${targetIp}
      proxied: true
outputs:
  hostname: ${record.hostname}
```

---

## Custom OpenTofu / Terraform Provider (`hashicorp/random` Example)

Any OpenTofu/Terraform provider can be added dynamically after Keel MGMT is running via PKO v2's `terraform-provider` bridge—without recompiling Keel. The bridge itself is exercised by the live E2E suite through the bundled `kind-cluster` blueprint (`tehcyx/kind`); this `hashicorp/random` example is not part of the automated tests. The zero-credential `hashicorp/random` example below is useful for smoke-testing the bridge end-to-end.

> **Resource Type Naming Tip:** Pulumi's dynamic `terraform-provider` bridge strips the `<provider>_` prefix from Terraform resource names (for example, `random_pet` becomes `random:index/pet:Pet`). Inspect exact resource type tokens anytime with:
> ```bash
> pulumi package get-schema terraform-provider hashicorp/random 3.6.3 | grep -i pet
> ```

### 1. Register the Blueprint After Keel MGMT Is Running

Open **Catalog & Form Wizard → Add Catalog Item → Infrastructure Blueprint (PKO v2)** in the Web UI (or `POST /api/v1/catalog`), set **Provider** to `kubernetes` (already allowed by `restrict-pulumi-providers` and enabled by default), set **Terraform Provider** to `hashicorp/random`, and paste the `catalog.yaml` schema / `Pulumi.yaml` below.

#### `catalog.yaml` (`catalog/infra/random-pet/catalog.yaml`)

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: random-pet
  name: Random Pet Generator
  description: Zero-credential random pet name via hashicorp/random OpenTofu/Terraform bridge.
  category: utilities
  provider: kubernetes
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - petId
  schema:
    type: object
    required: [prefix, length]
    properties:
      prefix:
        type: string
        title: Prefix
        default: keel
      length:
        type: integer
        title: Word Length
        default: 2
        minimum: 1
        maximum: 5
```

#### `Pulumi.yaml` (`catalog/infra/random-pet/Pulumi.yaml`)

```yaml
name: random-pet
runtime: yaml
description: Zero-credential random pet name via hashicorp/random
packages:
  random:
    source: terraform-provider
    parameters:
      - hashicorp/random
      - 3.6.3
config:
  prefix:
    type: string
    default: keel
  length:
    type: integer
    default: 2
resources:
  pet:
    type: random:index/pet:Pet
    properties:
      prefix: ${prefix}
      length: ${length}
outputs:
  petId: ${pet.id}
```

### 2. Merge & Provision

1. Merge the catalog registration PR in **PR Previews** (or `POST /api/v1/tenants/management/proposals/<pr_id>/merge`).
2. In **Catalog & Form Wizard**, click **Deploy** on **Random Pet Generator** (or run `keel infra provision --blueprint random-pet --tenant default --values-file values.yaml`).
3. Inspect the ephemeral PKO preview in **PR Previews**, then click **Approve & Merge**.
4. Flux CD creates `Stack/random-pet` in `tenant-default`, PKO v2 downloads the `terraform-provider` bridge for `hashicorp/random`, and `kubectl -n tenant-default get stack random-pet` reports `READY: True` with the generated `petId` output.
