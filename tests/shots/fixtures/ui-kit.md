### I-66：残りの証明を数えられるようになった

`proof_status` で I-65 Step 1 の残りを一覧にできる。

~~~fuzukue-v1
<fuzukue:card xmlns:fuzukue="urn:fuzukue:ui:v1" title="I-65 Step 1の証明状況">
  <fuzukue:grid columns="2">
    <fuzukue:stat label="部分証明あり" value="10" unit="件" tone="warn"/>
    <fuzukue:stat label="未証明" value="3" unit="件"/>
  </fuzukue:grid>
  <fuzukue:progress value="10" max="13" tone="warn" caption="13件のうち10件に部分証明あり（77%）"/>
  <fuzukue:divider/>
  <fuzukue:text style="heading">完全に未証明の3件</fuzukue:text>
  <fuzukue:list>
    <fuzukue:item>S01：一様有界性</fuzukue:item>
    <fuzukue:item>S05：誤差項の評価</fuzukue:item>
    <fuzukue:item>S09：極限と積分の交換</fuzukue:item>
  </fuzukue:list>
</fuzukue:card>
~~~

部分証明のある10件は、それぞれの残りの仮定を確認する。

OPEN：S01、S05、S09 の証明方針を決める。
