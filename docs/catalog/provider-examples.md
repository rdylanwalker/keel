# Provider Blueprint Examples

Each example below provides a ready-to-use `catalog.yaml` (`CatalogInfraBlueprint`) and `Pulumi.yaml` pair that you can place under `catalog/infra/<blueprint-id>/` in your GitOps repository and merge via PR.

Remember to enable the corresponding provider on the **Topology Map** (or in `providers/registry.yaml`) so the blueprint appears in **Catalog & Form Wizard**.

> **Credential & API Key Secrets:** Properties marked with `writeOnly: true` (and `format: password`) are partitioned out of plaintext `Stack.spec.config`, encrypted with SOPS + `age` into `secret.enc.yaml`, and injected into the PKO v2 workspace pod via `Stack.spec.envRefs` using the exact property name as the environment variable name.

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
        default: v1.32.0
        enum: [v1.32.0, v1.31.4, v1.30.8]
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
    default: v1.32.0
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
  nodeTopologies:
    - - role: control-plane
    - - role: control-plane
      - role: worker
    - - role: control-plane
      - role: worker
      - role: worker
    - - role: control-plane
      - role: worker
      - role: worker
      - role: worker
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

## Always-Free GCP Compute Engine VM (`gcp`)

Place in `catalog/infra/gcp-free-vm/`.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: gcp-free-vm
  name: Always-Free GCP Compute Engine VM
  description: Provision an Always Free e2-micro Compute Engine VM on Google Cloud.
  category: compute
  provider: gcp
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - instanceName
    - internalIp
  schema:
    type: object
    required: [vmName, project, zone, GOOGLE_CREDENTIALS]
    properties:
      vmName:
        type: string
        title: VM Name
        default: keel-free-vm
        pattern: "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$"
      project:
        type: string
        title: GCP Project ID
      zone:
        type: string
        title: GCP Zone (Always Free Eligible)
        default: us-east1-b
        enum:
          - us-central1-a
          - us-central1-b
          - us-central1-c
          - us-east1-b
          - us-east1-c
          - us-west1-a
          - us-west1-b
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
name: gcp-free-vm
runtime: yaml
description: Always Free GCP e2-micro Compute Engine VM
config:
  vmName:
    type: string
    default: keel-free-vm
  project:
    type: string
  zone:
    type: string
    default: us-east1-b
resources:
  freeVm:
    type: gcp:compute:Instance
    properties:
      name: ${vmName}
      project: ${project}
      machineType: e2-micro
      zone: ${zone}
      bootDisk:
        initializeParams:
          image: debian-cloud/debian-12
          size: 10
          type: pd-standard
      networkInterfaces:
        - network: default
      labels:
        managed-by: keel
outputs:
  instanceName: ${freeVm.name}
  internalIp: ${freeVm.networkInterfaces[0].networkIp}
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

## Proxmox VE k3s VM (`proxmox`)

Place in `catalog/infra/proxmox-k3s/`.

### `catalog.yaml`

```yaml
apiVersion: keel.io/v1alpha1
kind: CatalogInfraBlueprint
metadata:
  id: proxmox-k3s
  name: Proxmox VE k3s Node
  description: Provision a Proxmox VE virtual machine ready for k3s.
  category: compute
  provider: proxmox
spec:
  pulumiRuntime: yaml
  entrypoint: Pulumi.yaml
  protectByDefault: false
  outputs:
    - vmId
  schema:
    type: object
    required:
      - vmName
      - nodeName
      - cores
      - memoryMb
      - PROXMOX_VE_ENDPOINT
      - PROXMOX_VE_API_TOKEN
    properties:
      vmName:
        type: string
        title: VM Name
        default: k3s-worker-01
      nodeName:
        type: string
        title: Proxmox Node
        default: pve
      cores:
        type: integer
        title: vCPU Cores
        default: 4
        minimum: 1
        maximum: 32
      memoryMb:
        type: integer
        title: Memory (MB)
        default: 8192
        minimum: 1024
      PROXMOX_VE_ENDPOINT:
        type: string
        title: Proxmox VE API Endpoint
        writeOnly: true
      PROXMOX_VE_API_TOKEN:
        type: string
        title: Proxmox VE API Token
        format: password
        writeOnly: true
  uiSchema:
    PROXMOX_VE_ENDPOINT:
      ui:placeholder: "https://pve.example.local:8006/api2/json"
      ui:help: "Injected as PROXMOX_VE_ENDPOINT via Stack.spec.envRefs."
    PROXMOX_VE_API_TOKEN:
      ui:widget: password
      ui:help: "Format: user@realm!tokenid=uuid. Encrypted via SOPS + age."
```

### `Pulumi.yaml`

```yaml
name: proxmox-k3s
runtime: yaml
description: Proxmox VE VM via bpg/proxmox OpenTofu bridge
packages:
  proxmoxve:
    source: terraform-provider
    parameters:
      - bpg/proxmox
config:
  vmName:
    type: string
  nodeName:
    type: string
    default: pve
  cores:
    type: integer
    default: 4
  memoryMb:
    type: integer
    default: 8192
resources:
  vm:
    type: proxmoxve:vm:VirtualMachine
    properties:
      name: ${vmName}
      nodeName: ${nodeName}
      cpu:
        cores: ${cores}
      memory:
        dedicated: ${memoryMb}
outputs:
  vmId: ${vm.vmId}
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
