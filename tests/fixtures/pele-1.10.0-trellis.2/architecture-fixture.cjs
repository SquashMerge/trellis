"use strict";
function fixture() {
  const layers=[{name:"UI",order:0,shared:false},{name:"Entry",order:1,shared:false},{name:"Services",order:1,shared:false},{name:"Store",order:2,shared:false},{name:"Shared",order:null,shared:true}];
  const modules=Array.from({length:40},(_,i)=>({identity:{sourceSetId:"workspace",language:"rust",path:`src/module_${i}.rs`,sid:`file_${i}`},layer:layers[Math.min(4,Math.floor(i/8))].name,metrics:{cognitive:i},physicalLines:20}));
  const edges=[["UI","Entry",23,"production","conforms"],["Entry","Store",48,"production","conforms"],["Store","Entry",7,"production","deviates"],["Services","UI",4,"test","deviates"],["Services","Shared",9,"production","conforms"]].map(([sourceLayer,destinationLayer,count,role,status])=>({sourceLayer,destinationLayer,count,role,status,excepted:false,exceptionReason:null,pairs:[]}));
  return {state:"ready",indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1,layers,modules,edges};
}
module.exports={fixture};
